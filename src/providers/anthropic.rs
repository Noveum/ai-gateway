//! Anthropic provider adapter.
//!
//! Maps OpenAI-style `/v1/chat/completions` requests to Anthropic's
//! `/v1/messages` endpoint, converts the `Authorization: Bearer` header to
//! Anthropic's `x-api-key` + `anthropic-version`, and transforms the Messages
//! response (and streaming events) back into OpenAI shape. Token usage is read
//! from Anthropic's `usage.input_tokens`/`output_tokens` and priced via the
//! shared table.

use super::anthropic_stream;
use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::routing::transform_anthropic_to_openai_format;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderValue, Response},
};
use chrono;
use serde_json::Value;
use std::cell::RefCell;
use tracing::{debug, error};

thread_local! {
    static ANTHROPIC_INPUT_TOKENS: RefCell<Option<u32>> = const { RefCell::new(None) };
}

/// Provider adapter for Anthropic (`x-provider: anthropic`). Base URL
/// `https://api.anthropic.com`; rewrites the path to `/v1/messages` and the auth
/// header to `x-api-key`.
pub struct AnthropicProvider {
    base_url: String,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn transform_path(&self, path: &str) -> String {
        if path.contains("/chat/completions") {
            "/v1/messages".to_string()
        } else {
            path.to_string()
        }
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Anthropic request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Add Anthropic version header
        headers.insert(
            http::header::HeaderName::from_static("anthropic-version"),
            http::header::HeaderValue::from_static("2023-06-01"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Converting Bearer token to x-api-key format");
            let api_key = auth.trim_start_matches("Bearer ");
            headers.insert(
                http::header::HeaderName::from_static("x-api-key"),
                http::header::HeaderValue::from_str(api_key).map_err(|_| {
                    error!("Failed to process Anthropic authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Anthropic request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }

    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        // Clone response parts and body
        let (mut parts, body) = response.into_parts();

        // Check if it's a streaming response
        let is_streaming = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        // For streaming responses, we need to add the request ID header if it's available in other headers
        if is_streaming {
            // Anthropic sometimes includes a request-id header directly, let's check for it
            let request_id = parts
                .headers
                .get("request-id")
                .and_then(|v| v.to_str().ok())
                .map(|id| id.to_string());

            // If we found a request_id, add it as an x-request-id header
            if let Some(id) = request_id {
                debug!(
                    "Adding Anthropic request ID to streaming response headers: {}",
                    id
                );
                if let Ok(header_value) = HeaderValue::from_str(&id) {
                    parts.headers.insert("x-request-id", header_value);
                }
            } else {
                // For Anthropic, we might need to extract from the first streaming chunk
                // For now, we'll rely on the telemetry middleware to extract the request ID
                // from the streaming chunks and include it in the metrics
                debug!("No request-id header found for Anthropic streaming response");
            }

            // Only a successful event-stream is Anthropic's message protocol. A
            // non-2xx body is an error envelope and is passed through unchanged
            // (same rule as the non-streaming path below).
            if !parts.status.is_success() {
                debug!(
                    "Anthropic returned {} on a streaming request; passing the body through",
                    parts.status
                );
                return Ok(Response::from_parts(parts, body));
            }

            // Translate Anthropic's message events into OpenAI `chat.completion.chunk`
            // frames terminated by `data: [DONE]`. The transformer reassembles SSE
            // frames byte-exactly (transport chunks split frames at arbitrary
            // offsets, including mid-UTF-8), merges `input_tokens` from
            // `message_start` with `output_tokens` from `message_delta` into the
            // OpenAI `usage` object on the final chunk, and surfaces upstream
            // errors / truncation as a visible `event: error` frame instead of a
            // clean EOF that would look like a complete response.
            debug!("Transforming Anthropic event stream to OpenAI chunk format");
            let body = anthropic_stream::transform_body(body, chrono::Utc::now().timestamp());
            return Ok(Response::from_parts(parts, body));
        }

        // For regular responses, extract request_id from body and transform to OpenAI format
        let bytes = to_bytes(body, usize::MAX).await?;

        // Check if we have a request-id header in the response
        let request_id = parts
            .headers
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(|id| id.to_string());

        // If we found a request_id in the headers, add it as an x-request-id header
        if let Some(id) = request_id.clone() {
            debug!(
                "Adding Anthropic request ID from headers to response: {}",
                id
            );
            if let Ok(header_value) = HeaderValue::from_str(&id) {
                parts.headers.insert("x-request-id", header_value);
            }
        }

        // Always try to parse the response as JSON
        if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
            debug!("Successfully parsed response body as JSON: {:?}", json);

            // If we couldn't find a request_id in the headers, try to extract it from the body as a fallback
            let body_request_id = if request_id.is_none() {
                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                    Some(id.to_string())
                } else if let Some(message) = json.get("message") {
                    message
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| id.to_string())
                } else {
                    None
                }
            } else {
                None
            };

            // If we found a request_id in the body (and not in headers), add it as an x-request-id header
            if let Some(id) = body_request_id {
                debug!(
                    "Adding Anthropic request ID from body to response headers: {}",
                    id
                );
                if let Ok(header_value) = HeaderValue::from_str(&id) {
                    parts.headers.insert("x-request-id", header_value);
                }
            }

            // Extract the ID from the JSON response for later use
            let json_id = json.get("id").and_then(|v| v.as_str()).map(String::from);

            // Only convert SUCCESSFUL Messages responses to OpenAI shape. For
            // 4xx/5xx, preserve the upstream Anthropic error envelope + status
            // (matches the edge Worker) instead of emitting an empty
            // "chat.completion" payload.
            if !parts.status.is_success() {
                debug!(
                    "Anthropic returned {}; passing the error body through unchanged",
                    parts.status
                );
                return Ok(Response::from_parts(parts, Body::from(bytes)));
            }

            // Transform Anthropic API response to OpenAI format (shared with the
            // edge Worker; timestamp passed in since chrono is wasm-unavailable).
            let transformed_response =
                transform_anthropic_to_openai_format(json, chrono::Utc::now().timestamp());
            debug!("Transformed Anthropic response to OpenAI format");

            // Ensure x-request-id header is set in the response
            if !parts.headers.contains_key("x-request-id") {
                if let Some(id) = json_id {
                    debug!(
                        "Setting x-request-id header from Anthropic response ID: {}",
                        id
                    );
                    if let Ok(header_value) = HeaderValue::from_str(&id) {
                        parts.headers.insert("x-request-id", header_value);
                    }
                }
            }

            // Return the modified response
            return Ok(Response::from_parts(
                parts,
                Body::from(serde_json::to_vec(&transformed_response)?),
            ));
        } else {
            debug!("Failed to parse response body as JSON, returning original response");
        }

        // If we couldn't parse the JSON, return the original response
        Ok(Response::from_parts(parts, Body::from(bytes)))
    }
}

// Anthropic-specific metrics extractor
pub struct AnthropicMetricsExtractor;

impl MetricsExtractor for AnthropicMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!(
            "Extracting Anthropic metrics from response: {}",
            response_body
        );
        let mut metrics = ProviderMetrics::default();

        // Extract usage data
        if let Some(usage) = response_body.get("usage") {
            // Check for input tokens (Anthropic uses "prompt_tokens")
            metrics.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| {
                    usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32)
                });

            // Check for output tokens (Anthropic uses "completion_tokens")
            metrics.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| {
                    usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32)
                });

            // Get total tokens directly if available
            metrics.total_tokens = usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            // If total_tokens isn't available directly, calculate it from input and output
            if metrics.total_tokens.is_none() {
                // Calculate total tokens if both input and output tokens are available
                if let (Some(input), Some(output)) = (metrics.input_tokens, metrics.output_tokens) {
                    metrics.total_tokens = Some(input + output);
                }
                // If only one is available, use that as the total
                else if metrics.input_tokens.is_some() {
                    metrics.total_tokens = metrics.input_tokens;
                } else if metrics.output_tokens.is_some() {
                    metrics.total_tokens = metrics.output_tokens;
                }
            }
        }

        // Extract model information
        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            metrics.model = model.to_string();

            // Cost via the shared dual-rate pricing table.
            if let (Some(i), Some(o)) = (metrics.input_tokens, metrics.output_tokens) {
                let cost = crate::policy::pricing::estimate_cost(&metrics.model, i, o);
                if cost > 0.0 {
                    metrics.cost = Some(cost);
                }
            }
        }

        // Extract message ID to use as request ID
        if let Some(id) = response_body.get("id").and_then(|v| v.as_str()) {
            debug!("Found Anthropic message ID: {}", id);
            metrics.request_id = Some(id.to_string());
        } else if let Some(message) = response_body.get("message") {
            if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                debug!("Found Anthropic message ID in message object: {}", id);
                metrics.request_id = Some(id.to_string());
            }
        }

        debug!("Final extracted Anthropic metrics: {:?}", metrics);
        metrics
    }

    fn try_extract_provider_specific_streaming_metrics(
        &self,
        chunk: &str,
    ) -> Option<ProviderMetrics> {
        debug!(
            "Attempting to extract metrics from Anthropic streaming chunk: {}",
            chunk
        );

        // Try to parse the chunk as JSON
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // Get event type
            let event_type = json.get("type").and_then(|t| t.as_str())?;

            // Create metrics object
            let mut metrics = ProviderMetrics::default();
            metrics.model = "claude".to_string();

            // Extract model if available (from any event type)
            if let Some(model) = json
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
            {
                metrics.model = model.to_string();
            }

            // For message_start, extract and return input tokens
            if event_type == "message_start" {
                if let Some(message) = json.get("message") {
                    // Extract request ID
                    if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                        metrics.request_id = Some(id.to_string());
                    }

                    // Extract input tokens from usage section
                    if let Some(usage) = message.get("usage") {
                        if let Some(input_tokens) = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32)
                        {
                            metrics.input_tokens = Some(input_tokens);

                            // Store input tokens for later
                            ANTHROPIC_INPUT_TOKENS.with(|tokens| {
                                *tokens.borrow_mut() = Some(input_tokens);
                                debug!("Stored input tokens from message_start: {}", input_tokens);
                            });

                            return Some(metrics);
                        }
                    }
                }
            }
            // Final metrics are in message_delta with usage
            else if event_type == "message_delta" && json.get("usage").is_some() {
                let output_tokens = json
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .map(|t| t as u32);

                if let Some(output) = output_tokens {
                    metrics.output_tokens = Some(output);

                    // Get the stored input tokens
                    let input_tokens = ANTHROPIC_INPUT_TOKENS.with(|tokens| *tokens.borrow());
                    metrics.input_tokens = input_tokens;

                    // Calculate total tokens + cost (dual-rate via shared table).
                    let input = input_tokens.unwrap_or(0);
                    metrics.total_tokens = Some(input + output);
                    let cost = crate::policy::pricing::estimate_cost(&metrics.model, input, output);
                    if cost > 0.0 {
                        metrics.cost = Some(cost);
                    }
                    debug!(
                        "Final streaming metrics - input: {}, output: {}, total: {}",
                        input,
                        output,
                        input + output
                    );

                    return Some(metrics);
                }
            }
        }

        None
    }
}

// Convert Anthropic API response format to OpenAI format
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hdr(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert("authorization", a.parse().unwrap());
        }
        h
    }

    #[test]
    fn base_url_and_name() {
        let p = AnthropicProvider::new();
        assert_eq!(p.base_url(), "https://api.anthropic.com");
        assert_eq!(p.name(), "anthropic");
    }

    #[test]
    fn transform_path_maps_chat_completions_to_messages() {
        let p = AnthropicProvider::new();
        assert_eq!(p.transform_path("/v1/chat/completions"), "/v1/messages");
        // Non chat-completions paths are unchanged.
        assert_eq!(p.transform_path("/v1/models"), "/v1/models");
    }

    #[test]
    fn process_headers_converts_bearer_to_x_api_key_and_sets_version() {
        let p = AnthropicProvider::new();
        let out = p.process_headers(&hdr(Some("Bearer sk-ant-xyz"))).unwrap();
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-xyz");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(out.get(http::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn process_headers_missing_auth_errors() {
        let p = AnthropicProvider::new();
        assert!(matches!(
            p.process_headers(&hdr(None)),
            Err(AppError::MissingApiKey)
        ));
    }

    #[test]
    fn extract_metrics_reads_anthropic_usage_fields() {
        // Anthropic native shape: usage.input_tokens / output_tokens (no total).
        let body = json!({
            "model": "claude-sonnet-4-5-20250929",
            "usage": {"input_tokens": 100, "output_tokens": 40}
        });
        let m = AnthropicMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(100));
        assert_eq!(m.output_tokens, Some(40));
        assert_eq!(
            m.total_tokens,
            Some(140),
            "total computed from input+output"
        );
        // claude-sonnet-4-5 family: 3.0 in / 15.0 out per 1M
        let expected = (100.0 / 1e6) * 3.0 + (40.0 / 1e6) * 15.0;
        assert!((m.cost.unwrap() - expected).abs() < 1e-9);
    }

    /// The streamed usage must survive the Anthropic→OpenAI translation: the
    /// telemetry layer reads the *transformed* chunks (the provider runs before
    /// the metrics middleware), so cost/metering depends on the final chunk this
    /// transformer emits being readable by the extractor.
    #[test]
    fn streamed_usage_reaches_the_metrics_extractor() {
        use super::super::anthropic_stream::AnthropicStreamTransformer;

        let upstream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"model\":\"claude-sonnet-4-5-20250929\",\"usage\":{\"input_tokens\":100}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":40}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut transformer = AnthropicStreamTransformer::new(1_700_000_000);
        let mut sse = transformer.push(upstream.as_bytes()).sse;
        sse.push_str(&transformer.finish().sse);

        // Feed the transformed frames through the extractor exactly as the
        // telemetry middleware does, merging as it goes.
        let mut merged = ProviderMetrics::default();
        for payload in sse
            .split("\n\n")
            .filter_map(|f| f.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
        {
            if let Some(m) = AnthropicMetricsExtractor.extract_streaming_metrics(payload) {
                merged.merge_streaming(m);
            }
        }

        assert_eq!(merged.model, "claude-sonnet-4-5-20250929");
        assert_eq!(merged.input_tokens, Some(100), "from message_start");
        assert_eq!(merged.output_tokens, Some(40), "from message_delta");
        assert_eq!(merged.total_tokens, Some(140));
        assert_eq!(merged.request_id.as_deref(), Some("msg_9"));
        let expected = (100.0 / 1e6) * 3.0 + (40.0 / 1e6) * 15.0;
        assert!(
            (merged.cost.unwrap() - expected).abs() < 1e-9,
            "streamed cost must match the non-streaming price: {:?}",
            merged.cost
        );
    }

    #[test]
    fn transform_to_openai_shape_flattens_content_and_usage() {
        let anthropic = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5-20250929",
            "content": [{"type": "text", "text": "Hello "}, {"type": "text", "text": "world"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let out = transform_anthropic_to_openai_format(anthropic, 1_700_000_000);
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["created"], 1_700_000_000);
        assert_eq!(out["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 5);
        assert_eq!(out["usage"]["completion_tokens"], 2);
        assert_eq!(out["usage"]["total_tokens"], 7);
    }
}
