//! Anthropic provider adapter.
//!
//! Maps OpenAI-style `/v1/chat/completions` requests to Anthropic's
//! `/v1/messages` endpoint, converts the `Authorization: Bearer` header to
//! Anthropic's `x-api-key` + `anthropic-version`, and transforms the Messages
//! response (and streaming events) back into OpenAI shape. Token usage is read
//! from Anthropic's `usage.input_tokens`/`output_tokens` and priced via the
//! shared table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderValue, Response},
};
use chrono;
use serde_json::{json, Value};
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

            // Currently, we'll return the original streaming response without transformation
            // as transforming streams is complex and should be implemented more carefully

            // TODO: Implement proper streaming transformation for Anthropic to OpenAI format
            // This would require inspecting each chunk, transforming it to OpenAI format,
            // and reconstructing the stream. For now, we'll focus on regular responses.
            debug!("Returning streaming response without transformation");
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

            // Transform Anthropic API response to OpenAI format
            let transformed_response = transform_anthropic_to_openai_format(json);
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
fn transform_anthropic_to_openai_format(anthropic_response: Value) -> Value {
    // Extract content from Anthropic's array-based content structure
    let content =
        if let Some(content_array) = anthropic_response.get("content").and_then(|c| c.as_array()) {
            // Extract text content from content array (typically contains objects with "type" and "text")
            let mut text = String::new();
            for item in content_array {
                if let Some(item_text) = item.get("text").and_then(|t| t.as_str()) {
                    text.push_str(item_text);
                }
            }
            text
        } else {
            // Fallback if content structure is different
            anthropic_response
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string()
        };

    // Map Anthropic usage fields to OpenAI format
    let usage = {
        let mut usage_map = json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        });

        if let Some(anthropic_usage) = anthropic_response.get("usage") {
            // Map input_tokens to prompt_tokens
            if let Some(input_tokens) = anthropic_usage.get("input_tokens").and_then(|t| t.as_u64())
            {
                usage_map["prompt_tokens"] = json!(input_tokens);
            }

            // Map output_tokens to completion_tokens
            if let Some(output_tokens) = anthropic_usage
                .get("output_tokens")
                .and_then(|t| t.as_u64())
            {
                usage_map["completion_tokens"] = json!(output_tokens);
            }

            // Calculate total tokens
            let prompt_tokens = usage_map["prompt_tokens"].as_u64().unwrap_or(0);
            let completion_tokens = usage_map["completion_tokens"].as_u64().unwrap_or(0);
            usage_map["total_tokens"] = json!(prompt_tokens + completion_tokens);
        }

        usage_map
    };

    // Map stop_reason to finish_reason (Anthropic uses "end_turn", "max_tokens", etc.)
    let finish_reason = match anthropic_response
        .get("stop_reason")
        .and_then(|r| r.as_str())
    {
        Some("end_turn") => "stop",
        Some("max_tokens") => "length",
        Some("stop_sequence") => "stop",
        Some(reason) => reason,
        None => "stop", // Default
    };

    // Create OpenAI-compatible format
    let mut transformed = json!({
        "id": anthropic_response.get("id").unwrap_or(&Value::Null),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": anthropic_response.get("model").unwrap_or(&Value::Null),
        "type": anthropic_response.get("type").unwrap_or(&json!("message")),
        "role": anthropic_response.get("role").unwrap_or(&json!("assistant")),
        "choices": [{
            "index": 0,
            "message": {
                "role": anthropic_response.get("role").unwrap_or(&json!("assistant")),
                "content": content
            },
            "finish_reason": finish_reason
        }],
        "usage": usage,
        "system_fingerprint": format!("anthropic-{}", anthropic_response.get("model").and_then(|m| m.as_str()).unwrap_or("claude"))
    });

    // Handle the seed value - convert to string if present to avoid integer overflow in downstream stores
    if let Some(seed) = anthropic_response.get("seed") {
        if let Some(choices) = transformed
            .get_mut("choices")
            .and_then(|c| c.as_array_mut())
        {
            for choice in choices {
                if seed.is_number() {
                    // Convert the numeric seed to a string to avoid integer range issues in downstream stores
                    choice["seed"] = json!(seed.to_string());
                } else {
                    // If it's already a string or other type, just preserve it
                    choice["seed"] = seed.clone();
                }
            }
        }
    }

    transformed
}
