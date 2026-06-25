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

    // Process the stream
    tokio::spawn(async move {
        let mut response_size = 0;
        let mut accumulated_metrics = ProviderMetrics::default();
        let mut final_metrics_found = false;
        let mut resp_body = None;
        let mut streamed_chunks = Vec::new();

        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            if let Ok(bytes) = chunk {
                response_size += bytes.len();
                debug!("Streaming response chunk size: {} bytes", bytes.len());

                if let Ok(chunk_str) = String::from_utf8(bytes.to_vec()) {
                    if accumulated_text.len() + chunk_str.len() > MAX_ACCUMULATED_TEXT {
                        error!(
                            "Accumulated text exceeded maximum size of {} bytes",
                            MAX_ACCUMULATED_TEXT
                        );
                        break;
                    }
                    accumulated_text.push_str(&chunk_str);

                    // Try to parse the chunk as JSON and store it
                    if let Ok(json_chunk) = serde_json::from_str::<Value>(&chunk_str) {
                        // Only store non-empty chunks
                        if !json_chunk.is_null()
                            && !json_chunk.as_object().is_none_or(|o| o.is_empty())
                        {
                            streamed_chunks.push(json_chunk);
                        }
                    } else {
                        // For streaming that sends chunks broken up, try to parse
                        // different formats (like data: {...}\n\n for SSE)
                        for line in chunk_str.lines() {
                            if line.starts_with("data: ") {
                                let data = line.trim_start_matches("data: ");
                                if data == "[DONE]" {
                                    debug!("Received [DONE] signal in streaming");
                                    continue;
                                }

                                if let Ok(json_data) = serde_json::from_str::<Value>(data) {
                                    streamed_chunks.push(json_data.clone());

                                    // Try to extract metrics from this chunk
                                    if let Some(chunk_metrics) =
                                        metrics_extractor.extract_streaming_metrics(data)
                                    {
                                        debug!(
                                            "Found metrics in streaming chunk: {:?}",
                                            chunk_metrics
                                        );
                                        accumulated_metrics = chunk_metrics;
                                        final_metrics_found = true;
                                    }
                                }
                            }
                        }
                    }
                }

                // Always forward the bytes to the client
                if let Err(e) = tx.send(Ok(bytes)).await {
                    error!("Failed to forward streaming chunk: {}", e);
                    break;
                }
            } else if let Err(e) = chunk {
                error!("Error in streaming response: {}", e);
                // For type compatibility, we'll just break the stream instead of trying to send the error
                // This avoids issues with error type conversions
                break;
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

            // Rough token estimation based on accumulated text length
            // Approximation: ~4 characters per token for English text
            let estimated_output_tokens = if !accumulated_text.is_empty() {
                Some((accumulated_text.len() as f64 / 4.0).ceil() as u32)
            } else {
                None
            };

            debug!(
                "Estimated output tokens from {} bytes of text: {:?}",
                accumulated_text.len(),
                estimated_output_tokens
            );

            accumulated_metrics = ProviderMetrics {
                model,
                provider_latency: Duration::from_millis(0),
                output_tokens: estimated_output_tokens,
                // We don't have input tokens or total tokens
                ..Default::default()
            };

            final_metrics_found = true;
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
