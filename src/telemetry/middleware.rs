use super::metrics::MetricsRegistry;
use super::provider_metrics::{get_metrics_extractor, MetricsExtractor, ProviderMetrics};
use super::RequestMetrics;
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
use tracing::{debug, error};

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
        cost: provider_metrics.cost,
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

        let mut stream = body.into_data_stream();
        // HTTP/TCP chunk boundaries are arbitrary and have nothing to do with
        // SSE frame boundaries: a provider (or any hop) may flush `data: {...}`
        // split in the middle of the JSON, or even in the middle of a UTF-8
        // sequence. Parsing each transport chunk on its own silently dropped
        // those events — a whole stream could deliver perfectly valid usage to
        // the client while the gateway recorded nothing. Reassemble frames
        // first, then parse.
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
        let is_openai_streaming = provider == "openai";
        let is_groq_streaming = provider == "groq";
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
            if accumulated_metrics.cost.is_none() {
                if let (Some(i), Some(o)) = (
                    accumulated_metrics.input_tokens,
                    accumulated_metrics.output_tokens,
                ) {
                    let cost =
                        crate::policy::pricing::estimate_cost(&accumulated_metrics.model, i, o);
                    if cost > 0.0 {
                        accumulated_metrics.cost = Some(cost);
                    }
                }
            }
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

/// Incremental SSE frame reassembler.
///
/// Holds raw bytes that have not yet completed a frame and hands back only whole
/// frames (terminated by a blank line — `\n\n` or `\r\n\r\n`). Buffering at the
/// *byte* level is deliberate: a transport chunk can split a multi-byte UTF-8
/// character, so decoding per chunk would corrupt or discard it. Frame
/// terminators are ASCII, so a complete frame is always complete UTF-8.
#[derive(Default)]
struct SseFrameBuffer {
    buf: Vec<u8>,
}

impl SseFrameBuffer {
    /// Append transport bytes; return every frame they complete.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = Self::find_frame_end(&self.buf) {
            let frame = self.buf.drain(..end + sep_len).collect::<Vec<u8>>();
            out.push(String::from_utf8_lossy(&frame[..end]).into_owned());
        }
        // A frame that never terminates must not grow without bound; emit what
        // we have so the buffer stays capped.
        if self.buf.len() > MAX_ACCUMULATED_TEXT {
            let frame = std::mem::take(&mut self.buf);
            out.push(String::from_utf8_lossy(&frame).into_owned());
        }
        out
    }

    /// The unterminated remainder at end of stream, if any.
    fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let frame = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&frame).into_owned();
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Offset and length of the first frame terminator, if the buffer holds one.
    fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
        let find = |pat: &[u8]| buf.windows(pat.len()).position(|w| w == pat);
        match (find(b"\r\n\r\n"), find(b"\n\n")) {
            (Some(a), Some(b)) if a <= b => Some((a, 4)),
            (_, Some(b)) => Some((b, 2)),
            (Some(a), None) => Some((a, 4)),
            (None, None) => None,
        }
    }
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
