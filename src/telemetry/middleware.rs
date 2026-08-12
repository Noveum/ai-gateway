use super::metrics::MetricsRegistry;
use super::provider_metrics::{get_metrics_extractor, MetricsExtractor, ProviderMetrics};
use super::RequestMetrics;
use crate::policy::metering::SseFrameBuffer;
use crate::policy::pricing::CostBreakdown;
use axum::body::to_bytes;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{Request, Response},
    middleware::Next,
};
use futures_util::StreamExt;
use http;
use hyper::Error;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};

// Constants for safeguards
const CHANNEL_SIZE: usize = 1000; // Increased buffer for streaming response
const MAX_ACCUMULATED_TEXT: usize = 5 * 1024 * 1024; // 5MB limit

pub async fn metrics_middleware(
    State(registry): State<Arc<MetricsRegistry>>,
    req: Request<Body>,
    next: Next,
) -> Response<Body> {
    let start = Instant::now();

    // Extract provider and other request info
    let provider = req
        .headers()
        .get("x-provider")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("openai")
        .to_string();

    let path = req.uri().path().to_string();
    let method = req.method().to_string();

    // Extract project_id, org_id, and user_id from headers
    let project_id = req
        .headers()
        .get("x-project-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let org_id = req
        .headers()
        .get("x-organization-id")
        .or_else(|| req.headers().get("x-organisation-id")) // Try both British and American spellings
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let user_id = req
        .headers()
        .get("x-user-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let experiment_id = req
        .headers()
        .get("x-experiment-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Use our new utility method to extract tracking headers
    let tracking_headers = ProviderMetrics::extract_tracking_headers(req.headers());

    // Log the tracking headers at debug level for debugging
    debug!(
        "Tracking headers: project_id={:?}, organization_id={:?}, user_id={:?}, experiment_id={:?}",
        tracking_headers.project_id,
        tracking_headers.organization_id,
        tracking_headers.user_id,
        tracking_headers.experiment_id
    );

    debug!(
        "Received request: provider={}, path={}, method={}",
        provider, path, method
    );

    // Get metrics extractor for this provider
    let metrics_extractor = get_metrics_extractor(&provider);

    // Store original values before consuming request
    let original_method = req.method().clone();
    let original_uri = req.uri().clone();
    let original_headers = req.headers().clone();

    // Extract and store the original request body
    let (req_size, req_body, body) = {
        let bytes = to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let size = bytes.len();
        let req_body = serde_json::from_slice(&bytes).ok();
        debug!("Request body size: {} bytes", size);
        (size, req_body, Body::from(bytes))
    };

    // Reconstruct request with original values
    let mut new_req = Request::builder()
        .method(original_method)
        .uri(original_uri)
        .body(body)
        .unwrap();
    *new_req.headers_mut() = original_headers;

    // Process the response with a timeout
    let response = tokio::time::timeout(Duration::from_secs(30), next.run(new_req))
        .await
        .unwrap_or_else(|_| {
            debug!("Request timed out after 30 seconds");
            Response::builder()
                .status(http::StatusCode::GATEWAY_TIMEOUT)
                .body(Body::from("Request timed out after 30 seconds"))
                .unwrap()
        });

    let is_streaming = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    debug!("Response is streaming: {}", is_streaming);

    if is_streaming {
        handle_streaming_response(
            response,
            registry,
            provider,
            path,
            method,
            req_size,
            req_body,
            start,
            metrics_extractor,
            project_id,
            org_id,
            user_id,
            experiment_id,
        )
        .await
    } else {
        handle_regular_response(
            response,
            registry,
            provider,
            path,
            method,
            req_size,
            req_body,
            start,
            metrics_extractor,
            project_id,
            org_id,
            user_id,
            experiment_id,
        )
        .await
    }
}

async fn handle_regular_response(
    response: Response<Body>,
    registry: Arc<MetricsRegistry>,
    provider: String,
    path: String,
    method: String,
    req_size: usize,
    req_body: Option<Value>,
    start: Instant,
    metrics_extractor: Box<dyn MetricsExtractor>,
    project_id: Option<String>,
    org_id: Option<String>,
    user_id: Option<String>,
    experiment_id: Option<String>,
) -> Response<Body> {
    // Time to first byte is essentially the time taken to get the response headers
    let ttfb = start.elapsed();
    debug!("Time to first byte (TTFB): {:?}", ttfb);

    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
    let resp_size = bytes.len();

    debug!("Regular response body size: {} bytes", resp_size);

    // Extract provider request ID from response headers
    let provider_request_id = parts
        .headers
        .get("x-request-id")
        .or_else(|| parts.headers.get("request-id")) // Also check for Anthropic's request-id header
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(id) = &provider_request_id {
        debug!("Provider request ID: {}", id);
    }

    // Extract metrics from response body
    let (provider_metrics, resp_body) =
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            (metrics_extractor.extract_metrics(&json), Some(json))
        } else {
            (ProviderMetrics::default(), None)
        };

    debug!("Extracted provider metrics: {:?}", provider_metrics);

    let mut provider_metrics = provider_metrics;
    resolve_model_for_metering(&mut provider_metrics.model, req_body.as_ref());

    let (cost, cost_breakdown) = meter(
        provider_metrics.cost,
        &provider_metrics.model,
        &provider,
        resp_body.as_ref(),
        provider_metrics.input_tokens,
        provider_metrics.output_tokens,
    );

    let metrics = RequestMetrics {
        provider,
        path,
        method,
        model: provider_metrics.model,
        total_latency: start.elapsed(),
        provider_latency: provider_metrics.provider_latency,
        ttfb, // Add the TTFB measurement
        request_size: req_size,
        response_size: resp_size,
        input_tokens: provider_metrics.input_tokens,
        output_tokens: provider_metrics.output_tokens,
        total_tokens: provider_metrics.total_tokens,
        status_code: parts.status.as_u16(),
        cost,
        cost_breakdown,
        project_id: project_id.or(provider_metrics.project_id),
        org_id: org_id.or(provider_metrics.organization_id),
        user_id: user_id.or(provider_metrics.user_id),
        experiment_id: experiment_id.or(provider_metrics.experiment_id),
        provider_request_id,
        request_body: req_body,
        response_body: resp_body,
        guard_blocked: parts.headers.contains_key("x-noveum-guard-blocked"),
        ..Default::default()
    };

    registry.record_metrics(metrics).await;

    Response::from_parts(parts, Body::from(bytes))
}

async fn handle_streaming_response(
    response: Response<Body>,
    registry: Arc<MetricsRegistry>,
    provider: String,
    path: String,
    method: String,
    req_size: usize,
    req_body: Option<Value>,
    start: Instant,
    metrics_extractor: Box<dyn MetricsExtractor>,
    project_id: Option<String>,
    org_id: Option<String>,
    user_id: Option<String>,
    experiment_id: Option<String>,
) -> Response<Body> {
    // Time to first byte is essentially the time taken to get the response headers
    let ttfb = start.elapsed();
    debug!(
        "Time to first byte for streaming response (TTFB): {:?}",
        ttfb
    );

    let (parts, body) = response.into_parts();
    let (tx, rx) = mpsc::channel::<Result<Bytes, Error>>(CHANNEL_SIZE);

    // Extract provider request ID from response headers
    let provider_request_id = parts
        .headers
        .get("x-request-id")
        .or_else(|| parts.headers.get("request-id")) // Also check for Anthropic's request-id header
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(id) = &provider_request_id {
        debug!("Provider request ID for streaming response: {}", id);
    }

    let metrics_registry = registry.clone();
    let mut accumulated_text = String::with_capacity(MAX_ACCUMULATED_TEXT);
    // Guard blocks are never streamed (synthetic blocks are non-streaming), but
    // read the marker before `parts` is moved into the stream task for symmetry.
    let guard_blocked = parts.headers.contains_key("x-noveum-guard-blocked");

    // Process the stream
    tokio::spawn(async move {
        let mut response_size = 0;
        let mut accumulated_metrics = ProviderMetrics::default();
        let mut final_metrics_found = false;
        let mut resp_body = None;
        let mut streamed_chunks = Vec::new();
        let mut cost_breakdown = None;

        let mut stream = body.into_data_stream();
        // HTTP/TCP chunk boundaries are arbitrary and have nothing to do with
        // SSE frame boundaries: a provider (or any hop) may flush `data: {...}`
        // split in the middle of the JSON, or even in the middle of a UTF-8
        // sequence. Parsing each transport chunk on its own silently dropped
        // those events — a whole stream could deliver perfectly valid usage to
        // the client while the gateway recorded nothing. Reassemble frames
        // first, then parse. The reassembler lives in `policy::metering` so the
        // guard's streaming settlement, the Anthropic translator and this path
        // all share one proven implementation (and the Worker can too).
        let mut frames = SseFrameBuffer::default();
        // Set once the accumulation cap is hit: we stop *inspecting* but keep
        // forwarding bytes to the client (a metrics limit must never truncate
        // the user's response).
        let mut inspection_stopped = false;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    response_size += bytes.len();
                    debug!("Streaming response chunk size: {} bytes", bytes.len());

                    if !inspection_stopped {
                        for frame in frames.push(&bytes) {
                            if accumulated_text.len() + frame.len() + 2 > MAX_ACCUMULATED_TEXT {
                                error!(
                                    "Accumulated text exceeded maximum size of {} bytes",
                                    MAX_ACCUMULATED_TEXT
                                );
                                inspection_stopped = true;
                                break;
                            }
                            accumulated_text.push_str(&frame);
                            accumulated_text.push_str("\n\n");
                            ingest_stream_payload(
                                &frame,
                                metrics_extractor.as_ref(),
                                &mut streamed_chunks,
                                &mut accumulated_metrics,
                                &mut final_metrics_found,
                            );
                        }
                    }

                    // Always forward the bytes to the client
                    if let Err(e) = tx.send(Ok(bytes)).await {
                        error!("Failed to forward streaming chunk: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("Error in streaming response: {}", e);
                    // For type compatibility, we'll just break the stream instead of trying to send the error
                    // This avoids issues with error type conversions
                    break;
                }
            }
        }

        // A stream may end without the trailing blank line; whatever is left in
        // the buffer is the last frame.
        if !inspection_stopped {
            if let Some(tail) = frames.flush() {
                if accumulated_text.len() + tail.len() <= MAX_ACCUMULATED_TEXT {
                    accumulated_text.push_str(&tail);
                }
                ingest_stream_payload(
                    &tail,
                    metrics_extractor.as_ref(),
                    &mut streamed_chunks,
                    &mut accumulated_metrics,
                    &mut final_metrics_found,
                );
            }
        }

        // Try to parse the accumulated response
        if !accumulated_text.is_empty() {
            resp_body = serde_json::from_str(&accumulated_text).ok();
        }

        // Track if this is a provider that requires special streaming handling
        let (is_openai_streaming, is_groq_streaming) = streaming_metrics_quirks(&provider);
        let needs_special_streaming_handling = is_openai_streaming || is_groq_streaming;

        // For providers that don't always include token data in streaming responses,
        // create a minimal metrics record with what we know
        if !final_metrics_found && needs_special_streaming_handling && !streamed_chunks.is_empty() {
            // Try to extract model from the stream chunks
            let model = streamed_chunks
                .iter()
                .find_map(|chunk| chunk.get("model").and_then(|m| m.as_str()))
                .unwrap_or(if is_groq_streaming {
                    "llama"
                } else {
                    "unknown"
                })
                .to_string();

            debug!(
                "Creating partial metrics for {} streaming response with model: {}",
                provider, model
            );

            // Estimate output from the actual generated content (extracted
            // from the parsed chunks) — not the raw SSE envelope, whose JSON
            // framing would inflate the count by an order of magnitude.
            let estimated_output_tokens =
                estimate_stream_output_tokens(&streamed_chunks, &accumulated_text);

            // Estimate input tokens from the request body too — a stream with
            // no usage data must still report *some* real cost, not $0, or
            // platform cost caps silently never advance.
            let estimated_input_tokens = req_body
                .as_ref()
                .map(|rb| {
                    ProviderMetrics::estimate_tokens_from_text(&crate::routing::flatten_input_text(
                        rb,
                    ))
                })
                .filter(|&t| t > 0);

            debug!(
                "Estimated tokens for usage-less stream: input={:?} output={:?}",
                estimated_input_tokens, estimated_output_tokens
            );

            accumulated_metrics = ProviderMetrics {
                model,
                provider_latency: Duration::from_millis(0),
                input_tokens: estimated_input_tokens,
                output_tokens: estimated_output_tokens,
                ..Default::default()
            };

            final_metrics_found = true;
        }

        if final_metrics_found {
            // Any stream that ends without provider-reported token counts still
            // gets estimated ones (request text for input, accumulated SSE text
            // for output) — an unmetered call must not be reported as free.
            if accumulated_metrics.input_tokens.is_none() {
                accumulated_metrics.input_tokens = req_body
                    .as_ref()
                    .map(|rb| {
                        ProviderMetrics::estimate_tokens_from_text(
                            &crate::routing::flatten_input_text(rb),
                        )
                    })
                    .filter(|&t| t > 0);
            }
            if accumulated_metrics.output_tokens.is_none() {
                accumulated_metrics.output_tokens =
                    estimate_stream_output_tokens(&streamed_chunks, &accumulated_text);
            }

            // Streaming chunks often carry the model in one event and token
            // counts in another, so cost may not have been computable per-chunk
            // (e.g. Anthropic priced its `message_delta` under the placeholder
            // model "claude" → no cost). Recompute from the merged view.
            //
            // The billable dimensions live in whichever chunk carried `usage`
            // (the terminal one, by convention) — the accumulated SSE text is
            // not itself JSON, so the parsed chunks are the only place to find
            // cached and cache-written token counts in a stream.
            let usage_chunk = streamed_chunks
                .iter()
                .rev()
                .find(|c| c.get("usage").map(|u| u.is_object()).unwrap_or(false));
            // Streams are where a placeholder id survives: if no event ever named
            // the model, the extractor's substitute is still sitting here, and
            // metering it would price a call whose model was never learned.
            resolve_model_for_metering(&mut accumulated_metrics.model, req_body.as_ref());
            let (cost, breakdown) = meter(
                accumulated_metrics.cost,
                &accumulated_metrics.model,
                &provider,
                usage_chunk,
                accumulated_metrics.input_tokens,
                accumulated_metrics.output_tokens,
            );
            accumulated_metrics.cost = cost;
            cost_breakdown = breakdown;
        }

        // Record final metrics if we found them
        if final_metrics_found {
            let metrics = RequestMetrics {
                provider,
                path,
                method,
                model: accumulated_metrics.model,
                total_latency: start.elapsed(),
                provider_latency: accumulated_metrics.provider_latency,
                ttfb, // Add the TTFB measurement
                request_size: req_size,
                response_size,
                input_tokens: accumulated_metrics.input_tokens,
                output_tokens: accumulated_metrics.output_tokens,
                total_tokens: accumulated_metrics.total_tokens,
                status_code: parts.status.as_u16(),
                cost: accumulated_metrics.cost,
                cost_breakdown,
                project_id: project_id.or(accumulated_metrics.project_id),
                org_id: org_id.or(accumulated_metrics.organization_id),
                user_id: user_id.or(accumulated_metrics.user_id),
                experiment_id: experiment_id.or(accumulated_metrics.experiment_id),
                provider_request_id,
                request_body: req_body,
                response_body: resp_body,
                streamed_data: if !streamed_chunks.is_empty() {
                    Some(streamed_chunks)
                } else {
                    None
                },
                is_streaming: true,
                guard_blocked,
                ..Default::default()
            };
            metrics_registry.record_metrics(metrics).await;
        } else {
            debug!(
                "No final metrics found in streaming response. Total text accumulated: {} bytes",
                accumulated_text.len()
            );
        }
    });

    Response::from_parts(parts, Body::from_stream(ReceiverStream::new(rx)))
}

/// Parse one reassembled SSE frame (or a bare JSON payload) and fold whatever it
/// carries into the accumulating chunk list and metrics.
fn ingest_stream_payload(
    text: &str,
    metrics_extractor: &dyn MetricsExtractor,
    streamed_chunks: &mut Vec<Value>,
    accumulated_metrics: &mut ProviderMetrics,
    final_metrics_found: &mut bool,
) {
    // Some providers stream bare JSON objects rather than SSE `data:` lines.
    if let Ok(json_chunk) = serde_json::from_str::<Value>(text) {
        if !json_chunk.is_null() && !json_chunk.as_object().is_none_or(|o| o.is_empty()) {
            streamed_chunks.push(json_chunk);
        }
        return;
    }

    for line in text.lines() {
        // The space after `data:` is optional in the SSE spec.
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            debug!("Received [DONE] signal in streaming");
            continue;
        }
        if let Ok(json_data) = serde_json::from_str::<Value>(data) {
            streamed_chunks.push(json_data);

            // Try to extract metrics from this chunk
            if let Some(chunk_metrics) = metrics_extractor.extract_streaming_metrics(data) {
                debug!("Found metrics in streaming chunk: {:?}", chunk_metrics);
                // Merge, don't overwrite: providers spread model/input/output
                // across chunks.
                accumulated_metrics.merge_streaming(chunk_metrics);
                *final_metrics_found = true;
            }
        }
    }
}

/// Replace a placeholder model id with the one the client actually asked for.
///
/// Streaming extractors substitute a literal when an individual event carries no
/// `model` field - Groq's is `"llama"`, Anthropic's is `"claude"`. Usually a
/// later event names the real model and `merge_streaming` adopts it. When none
/// ever does, the placeholder survives to the metering boundary, and because it
/// matches nothing in the catalog it is priced as an *unknown model*: the
/// defensive catalog maximum of $15/$60 per 1M. For a `llama-3.3-70b` stream
/// that is roughly 50x the real rate.
///
/// The fix is to resolve the id rather than to weaken the assumption, which is
/// load-bearing for genuinely unknown models. The request body names the model -
/// it is the same value the guard middleware already admits and reserves
/// against - so metering and admission agree by construction rather than by
/// coincidence.
///
/// A request that names no model either leaves the placeholder in place: there
/// is nothing to resolve it to, and the conservative assumption is then the
/// right answer.
fn resolve_model_for_metering(model: &mut String, req_body: Option<&Value>) {
    if !ProviderMetrics::is_placeholder_model(model) {
        return;
    }
    let requested = req_body
        .and_then(|b| b.get("model"))
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !ProviderMetrics::is_placeholder_model(m));
    if let Some(requested) = requested {
        debug!(
            "Metering: no response event named the model (placeholder {:?}); \
             falling back to the requested model {:?}",
            model, requested
        );
        *model = requested.to_string();
    }
}

/// The cost to record for a call, given whatever the provider extractor could
/// work out. This is the metering boundary: the value returned here becomes
/// `RequestMetrics.cost`, i.e. the `costUsd` of the ALLOWED usage event that
/// the platform's cost caps and billing read.
///
/// A provider extractor leaves `cost: None` when the model is absent from the
/// pricing catalog (`estimate_cost` reports unpriced as `0.0`), which used to
/// make the whole call invisible to cost caps. Whenever token counts are known
/// we therefore price it here via [`crate::policy::pricing::price_call`], which
/// falls back to the assumed maximum catalog rate for an unknown model and logs
/// an alertable event. Only a call whose token counts are entirely unknown is
/// still recorded without a cost — there is nothing to price it from.
/// The itemized cost to record for a call, and the total to meter from it.
///
/// This supersedes [`backfill_cost`] wherever a response body is available,
/// because the body carries dimensions a token pair cannot express: a cached
/// prompt read bills at 0.02x-0.5x of input, a cache write at 1.25x-2x, and a
/// server-side search carries a per-request fee. Pricing those from
/// `(input_tokens, output_tokens)` alone charges every one of them at the plain
/// input rate or at nothing at all.
///
/// Falls back to the token pair when the body has no usage object, and to the
/// extractor's own figure when there are no token counts either. Returns
/// `(cost, breakdown)`; the breakdown rides along on [`RequestMetrics`] so the
/// usage exporter can post the components and the catalog version with it.
fn meter(
    reported: Option<f64>,
    model: &str,
    provider: &str,
    body: Option<&Value>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
) -> (Option<f64>, Option<CostBreakdown>) {
    let parsed = body
        .and_then(|b| crate::policy::pricing::parse_usage(model, provider, b))
        .filter(|u| !u.is_empty());
    let usage = match parsed {
        Some(u) => Some(u),
        None => match (input_tokens, output_tokens) {
            (None, None) => None,
            // One-sided counts still price the side we know, rather than nothing.
            (i, o) => Some(crate::policy::pricing::BillableUsage::from_tokens(
                i.unwrap_or(0),
                o.unwrap_or(0),
            )),
        },
    };
    let Some(usage) = usage else {
        return (
            backfill_cost(reported, model, input_tokens, output_tokens),
            None,
        );
    };
    let breakdown = crate::policy::pricing::price_usage(model, &usage);
    if !breakdown.is_complete {
        // `pricing` already emits one deduped alert per (model, dimension);
        // this records the concrete amount that alert is about.
        debug!(
            model = %model,
            provider = %provider,
            missing = ?breakdown.missing_dimension_names(),
            conservative_cost_usd = breakdown.total_usd,
            "Nova Guard: metering a conservative cost; a billable dimension could not be priced"
        );
    }
    (Some(breakdown.total_usd), Some(breakdown))
}

fn backfill_cost(
    reported: Option<f64>,
    model: &str,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
) -> Option<f64> {
    if let Some(c) = reported {
        return Some(c);
    }
    let (i, o) = match (input_tokens, output_tokens) {
        (None, None) => return None,
        // One-sided counts still price the side we know, rather than nothing.
        (i, o) => (i.unwrap_or(0), o.unwrap_or(0)),
    };
    let estimate = crate::policy::pricing::price_call(model, i, o);
    if estimate.is_assumed() {
        warn!(
            model = %model,
            input_tokens = i,
            output_tokens = o,
            assumed_cost_usd = estimate.usd,
            "Nova Guard: unpriced model metered at the assumed rate; verify the pricing catalog"
        );
    }
    Some(estimate.usd)
}

/// Whether `provider` is one whose streamed responses often omit a `usage`
/// block, so a stream that ends without provider-reported tokens must fall back
/// to estimated ones. Returns `(is_openai, is_groq)` — they differ only in the
/// placeholder model used when no chunk names one.
///
/// The `x-provider` header is forwarded verbatim, so this normalizes case the
/// same way [`get_metrics_extractor`] does. Comparing it as-is (the bug) meant
/// a client sending `x-provider: OpenAI` skipped the estimation path entirely
/// and every streamed response it made was billed as $0.
fn streaming_metrics_quirks(provider: &str) -> (bool, bool) {
    match provider.to_lowercase().as_str() {
        "openai" => (true, false),
        "groq" => (false, true),
        _ => (false, false),
    }
}

/// Estimate output tokens for a stream that ended without provider usage data.
/// Prefers the actual generated content — `choices[].delta.content` (and
/// `message.content`) text pulled from the parsed chunks — because estimating
/// from the raw accumulated SSE bytes would count the `data:` framing and JSON
/// envelope of every chunk as model output and overestimate by an order of
/// magnitude. Falls back to the raw text length only when no content could be
/// parsed at all.
fn estimate_stream_output_tokens(chunks: &[Value], accumulated_text: &str) -> Option<u32> {
    let content_chars: usize = chunks
        .iter()
        .filter_map(|c| c.get("choices").and_then(|v| v.as_array()))
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .or_else(|| choice.get("message"))
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.chars().count())
        .sum();
    if content_chars > 0 {
        Some((content_chars as f64 / 4.0).ceil() as u32)
    } else if !accumulated_text.is_empty() {
        Some((accumulated_text.len() as f64 / 4.0).ceil() as u32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_model_is_metered_instead_of_reported_free() {
        // The bug: a model absent from the pricing catalog left `cost: None`,
        // so the ALLOWED usage event carried $0 and cost caps never saw it.
        let assumed = crate::policy::pricing::assumed_unknown_price();
        let cost = backfill_cost(None, "some-brand-new-model", Some(1000), Some(500)).unwrap();
        let expected =
            (1000.0 / 1e6) * assumed.input_per_1m + (500.0 / 1e6) * assumed.output_per_1m;
        assert!((cost - expected).abs() < 1e-12, "metered {cost}");
        assert!(cost > 0.0);
        // A known model prices from the catalog, not from the assumption.
        let known = backfill_cost(None, "gpt-4o-mini", Some(1000), Some(500)).unwrap();
        assert!((known - ((1000.0 / 1e6) * 0.15 + (500.0 / 1e6) * 0.60)).abs() < 1e-12);
        assert!(known < cost);
        // A cost the provider already worked out is never second-guessed.
        assert_eq!(
            backfill_cost(Some(0.5), "made-up", Some(10), Some(10)),
            Some(0.5)
        );
        // One-sided token counts still price the side we know.
        assert!(backfill_cost(None, "made-up", Some(1000), None).unwrap() > 0.0);
        // Nothing to price from → no cost, rather than a fabricated one.
        assert_eq!(backfill_cost(None, "gpt-4o", None, None), None);
    }

    #[test]
    fn streaming_quirks_are_case_insensitive() {
        // `x-provider` is client-supplied and forwarded verbatim. Every casing
        // must behave exactly like the lowercase form — a case-sensitive
        // comparison silently dropped billing for the whole stream.
        let openai = streaming_metrics_quirks("openai");
        assert_eq!(openai, (true, false));
        for spelling in ["OpenAI", "OPENAI", "oPeNaI", "openAI", "Openai"] {
            assert_eq!(
                streaming_metrics_quirks(spelling),
                openai,
                "{spelling} must behave like `openai`"
            );
        }
        let groq = streaming_metrics_quirks("groq");
        assert_eq!(groq, (false, true));
        for spelling in ["Groq", "GROQ", "gRoQ"] {
            assert_eq!(
                streaming_metrics_quirks(spelling),
                groq,
                "{spelling} must behave like `groq`"
            );
        }
        // Unrelated providers keep the generic path, in any casing.
        for other in ["anthropic", "Anthropic", "bedrock", "", "openai-compatible"] {
            assert_eq!(streaming_metrics_quirks(other), (false, false), "{other}");
        }
    }

    #[test]
    fn stream_output_estimate_uses_content_not_sse_envelope() {
        // 3 chunks × 8 content chars = 24 chars ≈ 6 tokens, while the raw SSE
        // text is far larger — the estimate must come from the content.
        let chunks: Vec<Value> = (0..3)
            .map(|i| {
                json!({"id":"c","object":"chat.completion.chunk","model":"gpt-4o",
                       "choices":[{"index":0,"delta":{"content":"12345678"},"finish_reason":null}],
                       "padding": format!("irrelevant-envelope-bytes-{i}")})
            })
            .collect();
        let raw: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        let est = estimate_stream_output_tokens(&chunks, &raw).unwrap();
        assert_eq!(est, 6, "estimate from content chars, not envelope bytes");
        assert!(
            (raw.len() as f64 / 4.0).ceil() as u32 > est * 5,
            "envelope-based estimate would have been much larger"
        );
    }

    /// Feed `raw` through the reassembler in slices of `step` bytes (the way an
    /// adversarial provider or an unlucky TCP flush would), and return what the
    /// gateway would have recorded.
    fn collect_split(raw: &[u8], step: usize) -> (Vec<Value>, ProviderMetrics, bool) {
        let extractor = get_metrics_extractor("openai");
        let mut frames = SseFrameBuffer::default();
        let mut chunks = Vec::new();
        let mut metrics = ProviderMetrics::default();
        let mut found = false;
        for piece in raw.chunks(step.max(1)) {
            for frame in frames.push(piece) {
                ingest_stream_payload(
                    &frame,
                    extractor.as_ref(),
                    &mut chunks,
                    &mut metrics,
                    &mut found,
                );
            }
        }
        if let Some(tail) = frames.flush() {
            ingest_stream_payload(
                &tail,
                extractor.as_ref(),
                &mut chunks,
                &mut metrics,
                &mut found,
            );
        }
        (chunks, metrics, found)
    }

    fn sample_openai_stream() -> String {
        // A real-shaped OpenAI stream: content deltas (one carrying multi-byte
        // UTF-8), the final usage frame, then [DONE].
        concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Héllo\"}}]}\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" wörld 🌍\"}}]}\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5.6-luna\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
    }

    #[test]
    fn sse_frames_are_reassembled_across_transport_chunks() {
        let raw = sample_openai_stream();
        let bytes = raw.as_bytes();
        let (want_chunks, want_metrics, want_found) = collect_split(bytes, bytes.len());
        assert!(want_found, "unsplit stream must yield usage");
        assert_eq!(want_metrics.input_tokens, Some(11));
        assert_eq!(want_metrics.output_tokens, Some(4));

        // Every split size, including 1 byte at a time — which necessarily cuts
        // through the middle of the multi-byte characters and the JSON — must
        // produce byte-identical results to the unsplit stream.
        for step in 1..=bytes.len() {
            let (chunks, metrics, found) = collect_split(bytes, step);
            assert!(found, "usage lost when split every {step} bytes");
            assert_eq!(
                metrics.input_tokens, want_metrics.input_tokens,
                "input tokens differ at step {step}"
            );
            assert_eq!(
                metrics.output_tokens, want_metrics.output_tokens,
                "output tokens differ at step {step}"
            );
            assert_eq!(
                metrics.model, want_metrics.model,
                "model differs at step {step}"
            );
            assert_eq!(
                metrics.cost, want_metrics.cost,
                "cost differs at step {step}"
            );
            assert_eq!(chunks, want_chunks, "chunks differ at step {step}");
        }
    }

    #[test]
    fn sse_frames_split_at_every_single_byte_offset() {
        // The reviewer's exact reproduction: each valid frame flushed in two
        // halves. Sweep the cut point across the whole stream.
        let raw = sample_openai_stream();
        let bytes = raw.as_bytes();
        let (want_chunks, want_metrics, _) = collect_split(bytes, bytes.len());
        for cut in 1..bytes.len() {
            let extractor = get_metrics_extractor("openai");
            let mut frames = SseFrameBuffer::default();
            let (mut chunks, mut metrics, mut found) =
                (Vec::new(), ProviderMetrics::default(), false);
            for piece in [&bytes[..cut], &bytes[cut..]] {
                for frame in frames.push(piece) {
                    ingest_stream_payload(
                        &frame,
                        extractor.as_ref(),
                        &mut chunks,
                        &mut metrics,
                        &mut found,
                    );
                }
            }
            if let Some(tail) = frames.flush() {
                ingest_stream_payload(
                    &tail,
                    extractor.as_ref(),
                    &mut chunks,
                    &mut metrics,
                    &mut found,
                );
            }
            assert!(found, "usage lost when cut at byte {cut}");
            assert_eq!(chunks, want_chunks, "chunks differ when cut at byte {cut}");
            assert_eq!(
                (metrics.input_tokens, metrics.output_tokens),
                (want_metrics.input_tokens, want_metrics.output_tokens),
                "tokens differ when cut at byte {cut}"
            );
        }
    }

    #[test]
    fn sse_frames_handle_crlf_and_missing_trailing_blank_line() {
        // CRLF-framed stream (permitted by the SSE spec) with no terminator on
        // the final frame — the usage must still be recovered.
        let raw = "data: {\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\n\
                   data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}";
        let (chunks, metrics, found) = collect_split(raw.as_bytes(), 3);
        assert!(found);
        assert_eq!(chunks.len(), 2);
        assert_eq!(metrics.input_tokens, Some(7));
        assert_eq!(metrics.output_tokens, Some(2));
    }

    #[test]
    fn stream_output_estimate_falls_back_to_raw_text() {
        // No parseable content chunks → raw-length fallback; nothing → None.
        let est = estimate_stream_output_tokens(&[], "12345678");
        assert_eq!(est, Some(2));
        assert_eq!(estimate_stream_output_tokens(&[], ""), None);
    }
}

#[cfg(test)]
mod meter_tests {
    use super::*;
    use crate::policy::pricing::{BillableDimension, CostSource};
    use serde_json::json;

    /// The metering boundary reads the billable dimensions out of the response
    /// body, so a cached prompt is billed at the cached rate rather than at the
    /// input rate the token pair alone would imply.
    #[test]
    fn a_cached_openai_response_meters_below_the_token_pair_estimate() {
        let body = json!({
            "usage": {
                "prompt_tokens": 100_000,
                "completion_tokens": 1_000,
                "prompt_tokens_details": { "cached_tokens": 90_000 }
            }
        });
        let (cost, breakdown) = meter(
            None,
            "gpt-4o",
            "openai",
            Some(&body),
            Some(100_000),
            Some(1_000),
        );
        let b = breakdown.expect("a body with usage always produces a breakdown");
        assert!(b.is_complete);
        assert_eq!(b.source, CostSource::Catalog);
        // 10K uncached at $2.50/1M + 90K cached at $1.25/1M + 1K out at $10/1M.
        let expected = (10_000.0 / 1e6) * 2.50 + (90_000.0 / 1e6) * 1.25 + (1_000.0 / 1e6) * 10.00;
        assert!((cost.unwrap() - expected).abs() < 1e-12);
        // The old token-pair path would have charged every prompt token at the
        // full input rate.
        let naive = backfill_cost(None, "gpt-4o", Some(100_000), Some(1_000)).unwrap();
        assert!(cost.unwrap() < naive, "cached tokens must be discounted");
    }

    /// Anthropic's counters are exclusive, so the same body must ADD the cache
    /// dimensions rather than subtract them.
    #[test]
    fn an_anthropic_response_adds_its_exclusive_cache_counters() {
        let body = json!({
            "usage": {
                "input_tokens": 1_000,
                "output_tokens": 500,
                "cache_read_input_tokens": 50_000,
                "cache_creation_input_tokens": 10_000
            }
        });
        let (cost, breakdown) = meter(
            None,
            "claude-sonnet-4-5",
            "anthropic",
            Some(&body),
            Some(1_000),
            Some(500),
        );
        let b = breakdown.unwrap();
        assert!(b.is_complete);
        let expected = (1_000.0 / 1e6) * 3.00
            + (50_000.0 / 1e6) * 0.30
            + (10_000.0 / 1e6) * 3.75
            + (500.0 / 1e6) * 15.00;
        assert!((cost.unwrap() - expected).abs() < 1e-12);
        assert!(b.cache_read_usd > 0.0 && b.cache_write_usd > 0.0);
    }

    /// A dimension the catalog cannot price is charged conservatively and
    /// flagged, never dropped.
    #[test]
    fn an_unpriceable_dimension_meters_a_conservative_non_zero_cost() {
        let body = json!({
            "usage": {
                "prompt_tokens": 100_000,
                "completion_tokens": 0,
                "prompt_tokens_details": { "cached_tokens": 100_000 }
            }
        });
        let (cost, breakdown) = meter(None, "o1", "openai", Some(&body), Some(100_000), Some(0));
        let b = breakdown.unwrap();
        assert!(!b.is_complete);
        assert_eq!(b.missing_dimensions, vec![BillableDimension::CacheRead]);
        assert!(cost.unwrap() > 0.0, "must never meter a silent $0");
        assert!((cost.unwrap() - (100_000.0 / 1e6) * 15.00).abs() < 1e-12);
    }

    /// No usage object: fall back to the token pair, preserving the NOV-152
    /// behavior that an unknown model still meters at the assumed rate.
    #[test]
    fn a_body_without_usage_falls_back_to_the_token_pair() {
        let body = json!({ "choices": [] });
        let (cost, breakdown) = meter(
            None,
            "gpt-4o",
            "openai",
            Some(&body),
            Some(1_000),
            Some(500),
        );
        assert!(
            breakdown.is_some(),
            "token counts still produce a breakdown"
        );
        assert!(
            (cost.unwrap() - crate::policy::pricing::estimate_cost("gpt-4o", 1_000, 500)).abs()
                < 1e-12
        );

        // Nothing at all to price: the extractor's own figure survives and no
        // breakdown is invented.
        let (cost, breakdown) = meter(Some(0.5), "gpt-4o", "openai", None, None, None);
        assert_eq!(cost, Some(0.5));
        assert!(breakdown.is_none());

        // An unknown model with no body still meters defensively.
        let (cost, breakdown) = meter(
            None,
            "brand-new-model",
            "openai",
            None,
            Some(1_000),
            Some(500),
        );
        assert!(cost.unwrap() > 0.0);
        assert!(breakdown.unwrap().assumed_model_rate);
    }

    /// A per-request search fee has no token count at all, so only the body can
    /// surface it.
    #[test]
    fn per_request_search_fees_reach_the_meter() {
        let body = json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 100,
                "num_search_queries": 5,
                "search_context_size": "high"
            }
        });
        let (cost, breakdown) = meter(
            None,
            "sonar-pro",
            "perplexity",
            Some(&body),
            Some(100),
            Some(100),
        );
        let b = breakdown.unwrap();
        assert!((b.tool_usd - (5.0 / 1000.0) * 14.00).abs() < 1e-12);
        assert!(cost.unwrap() > b.tool_usd);
        // The token pair alone cannot see this fee at all.
        let tokens_only = backfill_cost(None, "sonar-pro", Some(100), Some(100)).unwrap();
        assert!(cost.unwrap() > tokens_only);
    }

    /// The Groq over-metering bug. A stream whose events never name the model
    /// leaves the extractor's placeholder in place; pricing it as an unknown
    /// model charges the catalog maximum, ~50x a real llama rate.
    #[test]
    fn a_placeholder_model_resolves_to_the_requested_one() {
        use serde_json::json;

        let req = json!({"model": "llama-3.3-70b-versatile", "stream": true});

        // Every placeholder the extractors substitute is resolved.
        for placeholder in ["", "unknown", "llama", "claude", "  "] {
            let mut model = placeholder.to_string();
            resolve_model_for_metering(&mut model, Some(&req));
            assert_eq!(
                model, "llama-3.3-70b-versatile",
                "placeholder {placeholder:?} must resolve to the requested model"
            );
        }

        // A model the response DID name is authoritative and must not be
        // overwritten by the request - a provider may serve an alias.
        let mut model = "llama-3.1-8b-instant".to_string();
        resolve_model_for_metering(&mut model, Some(&req));
        assert_eq!(model, "llama-3.1-8b-instant");

        // Nothing to resolve to: the placeholder stands, and the defensive
        // unknown-model assumption is then the correct answer.
        for body in [None, Some(json!({})), Some(json!({"model": "unknown"}))] {
            let mut model = "llama".to_string();
            resolve_model_for_metering(&mut model, body.as_ref());
            assert_eq!(model, "llama", "body {body:?} carries no model to use");
        }
    }

    /// The resolved id is what pricing sees, so the bug is fixed where it hurt:
    /// the metered cost.
    #[test]
    fn resolving_the_placeholder_prices_the_real_model_not_the_assumption() {
        use serde_json::json;

        let assumed = crate::policy::pricing::price_call("llama", 1000, 1000).usd;
        let mut model = "llama".to_string();
        resolve_model_for_metering(&mut model, Some(&json!({"model": "gpt-4o"})));
        let resolved = crate::policy::pricing::price_call(&model, 1000, 1000).usd;

        assert!(
            resolved < assumed,
            "resolving must lower the cost from the catalog maximum: \
             assumed {assumed}, resolved {resolved}"
        );
    }
}
