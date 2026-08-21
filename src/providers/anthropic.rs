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
use crate::anthropic_stream;
use crate::error::AppError;
use crate::routing::{
    authorization_bearer_token, openai_to_anthropic_messages, transform_anthropic_to_openai_format,
};
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use axum::{
    body::{to_bytes, Body, Bytes},
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
        let base_url = crate::routing::normalize_base_url(
            std::env::var(crate::routing::ANTHROPIC_BASE_URL_VAR)
                .ok()
                .as_deref(),
        )
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self { base_url }
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

        // Preserve upstream feature flags, tracing, and explicitly supplied
        // custom headers. Strip transport/auth headers (rebuilt below) and
        // gateway-internal tenancy/guard metadata so it cannot leak upstream.
        // RFC 9110 also lets `Connection` name additional hop-by-hop fields;
        // collect those names before discarding the header itself.
        let connection_tokens = original_headers
            .get_all(http::header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        for (name, value) in original_headers {
            let name_str = name.as_str();
            let skip = matches!(
                name_str,
                "host"
                    | "content-length"
                    | "content-encoding"
                    | "accept-encoding"
                    | "connection"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "proxy-connection"
                    | "te"
                    | "trailer"
                    | "upgrade"
                    | "authorization"
                    | "x-api-key"
                    | "content-type"
                    | "anthropic-version"
                    | "x-provider"
                    | "x-project-id"
                    | "x-organization-id"
                    | "x-organisation-id"
                    | "x-user-id"
                    | "x-experiment-id"
                    | "cookie"
            ) || name_str.starts_with("x-noveum-")
                || name_str.starts_with("x-aws-")
                || connection_tokens.iter().any(|token| token == name_str);
            if !skip {
                headers.append(name.clone(), value.clone());
            }
        }

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

        // Accept the OpenAI SDK's Bearer convention and Anthropic's native
        // x-api-key convention. Never forward an arbitrary Authorization
        // scheme as an Anthropic key.
        let bearer_key = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(authorization_bearer_token);
        let native_key = original_headers
            .get("x-api-key")
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(api_key) = bearer_key.or(native_key) {
            debug!("Forwarding Anthropic API key as x-api-key");
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

    async fn prepare_request_body(&self, body: Bytes) -> Result<Bytes, AppError> {
        let request: Value = serde_json::from_slice(&body)?;
        let request = openai_to_anthropic_messages(request).map_err(AppError::RequestError)?;
        Ok(Bytes::from(serde_json::to_vec(&request)?))
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
            parts.headers.remove(http::header::CONTENT_LENGTH);
            parts.headers.remove(http::header::CONTENT_ENCODING);
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
            parts.headers.remove(http::header::CONTENT_LENGTH);
            parts.headers.remove(http::header::CONTENT_ENCODING);
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
            let as_u32_saturating = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
            // Check for input tokens (Anthropic uses "prompt_tokens")
            metrics.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .map(as_u32_saturating)
                .or_else(|| {
                    usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .map(as_u32_saturating)
                });

            // Check for output tokens (Anthropic uses "completion_tokens")
            metrics.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .map(as_u32_saturating)
                .or_else(|| {
                    usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .map(as_u32_saturating)
                });

            // Get total tokens directly if available
            metrics.total_tokens = usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .map(as_u32_saturating);

            // If total_tokens isn't available directly, calculate it from input and output
            if metrics.total_tokens.is_none() {
                // Calculate total tokens if both input and output tokens are available
                if let (Some(input), Some(output)) = (metrics.input_tokens, metrics.output_tokens) {
                    metrics.total_tokens = Some(input.saturating_add(output));
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

            // Use the same detailed pricing path as NovaGuard settlement so
            // cache writes/reads, data residency, server tools, and unbilled
            // pre-output refusals cannot disagree with telemetry.
            if let Some(usage) =
                crate::policy::pricing::parse_usage(&metrics.model, "anthropic", response_body)
            {
                metrics.input_tokens = Some(usage.total_input_tokens());
                metrics.output_tokens = Some(usage.output_tokens);
                metrics.total_tokens = Some(
                    usage
                        .total_input_tokens()
                        .saturating_add(usage.output_tokens),
                );
                metrics.cost =
                    Some(crate::policy::pricing::price_usage(&metrics.model, &usage).total_usd);
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
                            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
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
                    .map(|t| u32::try_from(t).unwrap_or(u32::MAX));

                if let Some(output) = output_tokens {
                    metrics.output_tokens = Some(output);

                    // Get the stored input tokens
                    let input_tokens = ANTHROPIC_INPUT_TOKENS.with(|tokens| *tokens.borrow());
                    metrics.input_tokens = input_tokens;

                    // Calculate total tokens + cost (dual-rate via shared table).
                    let input = input_tokens.unwrap_or(0);
                    metrics.total_tokens = Some(input.saturating_add(output));
                    let cost = crate::policy::pricing::estimate_cost(&metrics.model, input, output);
                    if cost > 0.0 {
                        metrics.cost = Some(cost);
                    }
                    debug!(
                        "Final streaming metrics - input: {}, output: {}, total: {}",
                        input,
                        output,
                        input.saturating_add(output)
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
    fn process_headers_accepts_native_anthropic_x_api_key() {
        let p = AnthropicProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "sk-ant-native".parse().unwrap());
        let out = p.process_headers(&headers).unwrap();
        assert_eq!(out.get("x-api-key").unwrap(), "sk-ant-native");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn process_headers_forwards_anthropic_features_and_tracing_without_internal_headers() {
        let p = AnthropicProvider::new();
        let mut headers = hdr(Some("Bearer sk-ant-xyz"));
        headers.insert(
            "anthropic-beta",
            "interleaved-thinking-2025-05-14".parse().unwrap(),
        );
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        headers.insert("x-provider", "anthropic".parse().unwrap());
        headers.insert("x-project-id", "internal-project".parse().unwrap());
        headers.insert("proxy-authorization", "Basic secret".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("x-noveum-future-internal", "private".parse().unwrap());
        headers.insert("connection", "x-remove-me".parse().unwrap());
        headers.insert("x-remove-me", "hop-by-hop".parse().unwrap());

        let out = p.process_headers(&headers).unwrap();
        assert_eq!(
            out.get("anthropic-beta").unwrap(),
            "interleaved-thinking-2025-05-14"
        );
        assert!(out.get("traceparent").is_some());
        assert!(out.get("x-provider").is_none());
        assert!(out.get("x-project-id").is_none());
        assert!(out.get("proxy-authorization").is_none());
        assert!(out.get("upgrade").is_none());
        assert!(out.get("x-noveum-future-internal").is_none());
        assert!(out.get("x-remove-me").is_none());
    }

    #[test]
    fn process_headers_missing_auth_errors() {
        let p = AnthropicProvider::new();
        assert!(matches!(
            p.process_headers(&hdr(None)),
            Err(AppError::MissingApiKey)
        ));
    }

    #[tokio::test]
    async fn prepare_request_body_uses_the_shared_anthropic_normalizer() {
        let p = AnthropicProvider::new();
        let input = json!({
            "model": "claude-sonnet-4-5",
            "max_completion_tokens": 64,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [
                {"role": "developer", "content": "Be concise"},
                {"role": "user", "content": "Hello"}
            ]
        });

        let bytes = p
            .prepare_request_body(axum::body::Bytes::from(serde_json::to_vec(&input).unwrap()))
            .await
            .unwrap();
        let forwarded: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(forwarded["max_tokens"], 64);
        assert_eq!(forwarded["system"], "Be concise");
        assert_eq!(
            forwarded["messages"],
            json!([{"role": "user", "content": "Hello"}])
        );
        assert!(forwarded.get("max_completion_tokens").is_none());
        assert!(forwarded.get("stream_options").is_none());
    }

    #[tokio::test]
    async fn prepare_request_body_returns_a_stable_client_error_for_invalid_messages() {
        let p = AnthropicProvider::new();
        let error = p
            .prepare_request_body(axum::body::Bytes::from_static(br#"{"messages":42}"#))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::RequestError(message) if message == "messages must be an array"
        ));
    }

    #[tokio::test]
    async fn prepare_request_body_rejects_mcp_and_server_tools_via_the_shared_preflight() {
        let p = AnthropicProvider::new();
        let cases = [
            (
                json!({
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "mcp_servers": [{"type": "url", "url": "https://mcp.example"}]
                }),
                "mcp_servers",
            ),
            (
                json!({
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": [{
                        "type": "web_search_20250305",
                        "name": "web_search",
                        "input_schema": {"type": "object"}
                    }]
                }),
                "web_search_20250305",
            ),
        ];

        for (input, expected) in cases {
            let error = p
                .prepare_request_body(Bytes::from(serde_json::to_vec(&input).unwrap()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, AppError::RequestError(message) if message.contains(expected)),
                "the native provider hook must surface the shared {expected} rejection"
            );
        }
    }

    #[tokio::test]
    async fn transformed_response_drops_stale_length_and_encoding_headers() {
        let p = AnthropicProvider::new();
        let raw = json!({
            "id": "msg_headers",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [{"type":"text","text":"hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 2, "output_tokens": 1}
        })
        .to_string();
        let response = Response::builder()
            .status(200)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::CONTENT_LENGTH, raw.len().to_string())
            .header(http::header::CONTENT_ENCODING, "gzip")
            .body(Body::from(raw))
            .unwrap();

        let transformed = p.process_response(response).await.unwrap();
        assert!(transformed
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .is_none());
        assert!(transformed
            .headers()
            .get(http::header::CONTENT_ENCODING)
            .is_none());
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

        let huge = AnthropicMetricsExtractor.extract_metrics(&json!({
            "model": "claude-sonnet-5",
            "usage": {"input_tokens": u64::MAX, "output_tokens": u64::MAX}
        }));
        assert_eq!(huge.input_tokens, Some(u32::MAX));
        assert_eq!(huge.output_tokens, Some(u32::MAX));
        assert_eq!(huge.total_tokens, Some(u32::MAX));

        let detailed = json!({
            "model": "claude-sonnet-5",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 50,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 10,
                    "ephemeral_1h_input_tokens": 40
                },
                "inference_geo": "us"
            }
        });
        let m = AnthropicMetricsExtractor.extract_metrics(&detailed);
        assert_eq!(m.input_tokens, Some(180), "all disjoint input dimensions");
        let usage =
            crate::policy::pricing::parse_usage("claude-sonnet-5", "anthropic", &detailed).unwrap();
        let expected = crate::policy::pricing::price_usage("claude-sonnet-5", &usage).total_usd;
        assert!((m.cost.unwrap() - expected).abs() < 1e-12);

        let refusal = AnthropicMetricsExtractor.extract_metrics(&json!({
            "model": "claude-sonnet-5",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 0,
                "unbilled_refusal": true
            }
        }));
        assert_eq!(refusal.cost, Some(0.0));
    }

    /// The streamed usage must survive the Anthropic→OpenAI translation: the
    /// telemetry layer reads the *transformed* chunks (the provider runs before
    /// the metrics middleware), so cost/metering depends on the final chunk this
    /// transformer emits being readable by the extractor.
    #[test]
    fn streamed_usage_reaches_the_metrics_extractor() {
        use crate::anthropic_stream::AnthropicStreamTransformer;

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
