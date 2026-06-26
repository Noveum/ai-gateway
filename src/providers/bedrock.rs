//! AWS Bedrock provider adapter.
//!
//! Translates OpenAI-style chat requests into Bedrock's Converse API
//! (`/model/{id}/converse[-stream]`), signs them with AWS SigV4 (credentials and
//! region taken from request headers), and transforms Converse responses and
//! event-stream chunks back into OpenAI shape. Token usage is read from Bedrock's
//! `usage.inputTokens`/`outputTokens` (with an OpenAI-shape fallback).

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use aws_event_stream_parser::{parse_message, Message};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode},
};
use futures_util::StreamExt;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, error, warn};
use uuid;

/// Constants for default values
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MODEL: &str = "amazon.titan-text-premier-v1:0";
const DEFAULT_MAX_TOKENS: u64 = 1000;
const DEFAULT_TEMPERATURE: f64 = 0.7;
const DEFAULT_TOP_P: f64 = 1.0;

/// BedrockProvider handles AWS Bedrock API integration
#[derive(Clone)]
pub struct BedrockProvider {
    base_url: Arc<RwLock<String>>,
    region: Arc<RwLock<String>>,
    current_model: Arc<RwLock<String>>,
    is_streaming: Arc<RwLock<bool>>,
    system_fingerprint: Arc<RwLock<String>>,
    first_chunk: Arc<RwLock<bool>>,
}

impl Default for BedrockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BedrockProvider {
    pub fn new() -> Self {
        let region = DEFAULT_REGION.to_string();
        debug!("Initializing BedrockProvider with region: {}", region);

        // Create a random system fingerprint that will be reused across chunks
        let fingerprint = format!(
            "fp_{}",
            uuid::Uuid::new_v4()
                .to_string()
                .replace("-", "")
                .chars()
                .take(8)
                .collect::<String>()
        );

        Self {
            base_url: Arc::new(RwLock::new(format!(
                "https://bedrock-runtime.{}.amazonaws.com",
                region
            ))),
            region: Arc::new(RwLock::new(region)),
            current_model: Arc::new(RwLock::new(DEFAULT_MODEL.to_string())),
            is_streaming: Arc::new(RwLock::new(false)),
            system_fingerprint: Arc::new(RwLock::new(fingerprint)),
            first_chunk: Arc::new(RwLock::new(true)),
        }
    }

    fn transform_request_body(&self, body: Value) -> Result<Value, AppError> {
        debug!("Transforming request body: {:#?}", body);

        // Return early if already in correct format
        if body.get("inferenceConfig").is_some() {
            return Ok(body);
        }

        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                error!("Invalid request format: messages array not found");
                AppError::InvalidRequestFormat
            })?;

        let transformed_messages = messages
            .iter()
            .map(|msg| {
                let content = msg["content"].as_str().unwrap_or_default();
                json!({
                    "role": msg["role"].as_str().unwrap_or("user"),
                    "content": [{ "text": content }]
                })
            })
            .collect::<Vec<_>>();

        let transformed = json!({
            "messages": transformed_messages,
            "inferenceConfig": {
                "maxTokens": body.get("max_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_MAX_TOKENS),
                "temperature": body.get("temperature")
                    .and_then(Value::as_f64)
                    .unwrap_or(DEFAULT_TEMPERATURE),
                "topP": body.get("top_p")
                    .and_then(Value::as_f64)
                    .unwrap_or(DEFAULT_TOP_P)
            }
        });

        debug!("Transformed body: {:#?}", transformed);
        Ok(transformed)
    }

    fn transform_bedrock_chunk(&self, chunk: Bytes) -> Result<Bytes, AppError> {
        debug!("Processing chunk of size: {}", chunk.len());
        let mut remaining = chunk.as_ref();
        let mut response_events = Vec::new();

        while !remaining.is_empty() {
            match self.process_message(remaining) {
                Ok((rest, events)) => {
                    remaining = rest;
                    response_events.extend(events);
                }
                Err(e) => {
                    debug!("Failed to parse message: {:?}", e);
                    break;
                }
            }
        }

        Ok(Bytes::from(response_events.join("")))
    }

    fn process_message<'a>(&self, data: &'a [u8]) -> Result<(&'a [u8], Vec<String>), AppError> {
        let (rest, message) =
            parse_message(data).map_err(|e| AppError::EventStreamError(e.to_string()))?;

        let event_type = self.get_event_type(&message);
        let events = match event_type.as_deref() {
            Some("contentBlockDelta") => self.handle_content_block(&message)?,
            Some("metadata") => self.handle_metadata(&message)?,
            _ => {
                debug!("Skipping event type: {:?}", event_type);
                vec![]
            }
        };

        if !message.valid() {
            warn!("Invalid message checksum detected");
        }

        Ok((rest, events))
    }

    fn get_event_type(&self, message: &Message) -> Option<String> {
        message
            .headers
            .headers
            .iter()
            .find(|h| h.key == ":event-type")
            .and_then(|h| match &h.value {
                aws_event_stream_parser::HeaderValue::String(s) => Some(s.to_string()),
                _ => None,
            })
    }

    /// Handles content block chunks from Bedrock and transforms them to the OpenAI streaming format.
    ///
    /// For the first chunk, this includes the "role": "assistant" field in the delta.
    /// For all chunks, this includes the same system_fingerprint and required OpenAI fields
    /// for compatibility with OpenAI SDKs.
    fn handle_content_block(&self, message: &Message) -> Result<Vec<String>, AppError> {
        let body_str = String::from_utf8(message.body.to_vec())?;
        let json: Value = serde_json::from_str(&body_str)?;

        if let Some(delta) = json
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
        {
            let response = self.create_delta_response(delta);
            Ok(vec![format!("data: {}\n\n", response.to_string())])
        } else {
            Ok(vec![])
        }
    }

    /// Handles metadata chunks from Bedrock (typically the final chunk) and transforms them
    /// to the OpenAI streaming format.
    ///
    /// The final chunk includes usage information and a finish_reason of "stop".
    /// This also includes the [DONE] marker required by OpenAI's streaming protocol.
    fn handle_metadata(&self, message: &Message) -> Result<Vec<String>, AppError> {
        let body_str = String::from_utf8(message.body.to_vec())?;
        let json: Value = serde_json::from_str(&body_str)?;

        if let Some(usage) = json.get("usage") {
            let final_message = self.create_final_response(usage);
            Ok(vec![format!(
                "data: {}\ndata: [DONE]\n\n",
                final_message.to_string()
            )])
        } else {
            Ok(vec![])
        }
    }

    fn create_delta_response(&self, delta: &str) -> Value {
        let mut delta_content = json!({
            "content": delta
        });

        // For the first chunk, include the role: "assistant"
        let is_first = {
            let mut first = self.first_chunk.write();
            let was_first = *first;
            if was_first {
                *first = false; // Update for next chunk
                true
            } else {
                false
            }
        };

        if is_first {
            delta_content["role"] = json!("assistant");
        }

        json!({
            "id": "chatcmpl-bedrock",
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.current_model.read().as_str(),
            "choices": [{
                "index": 0,
                "delta": delta_content,
                "finish_reason": null
            }],
            "service_tier": "default",
            "system_fingerprint": self.system_fingerprint.read().clone()
        })
    }

    fn create_final_response(&self, usage: &Value) -> Value {
        // Extract usage data and transform to OpenAI format
        let input_tokens = usage
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total_tokens = usage
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        // Create transformed usage object
        let transformed_usage = json!({
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": total_tokens
        });

        json!({
            "id": "chatcmpl-bedrock",
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.current_model.read().as_str(),
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": transformed_usage,
            "service_tier": "default",
            "system_fingerprint": self.system_fingerprint.read().clone()
        })
    }

    // Helper method to transform Bedrock response to OpenAI format
    fn transform_bedrock_to_openai_format(
        &self,
        bedrock_response: Value,
    ) -> Result<Value, AppError> {
        debug!("Transforming Bedrock response to OpenAI format");

        // Extract content from Bedrock response
        let content = bedrock_response
            .get("output")
            .and_then(|output| output.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        // Extract usage metrics
        let usage = bedrock_response.get("usage").cloned().unwrap_or_else(|| {
            json!({
                "inputTokens": 0,
                "outputTokens": 0,
                "totalTokens": 0
            })
        });

        // Get stop reason
        let finish_reason = bedrock_response
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("stop");

        // Map Bedrock finish reason to OpenAI format
        let openai_finish_reason = match finish_reason {
            "end_turn" => "stop",
            "max_tokens" => "length",
            "stop_sequence" => "stop",
            _ => "stop",
        };

        // Create OpenAI format response
        let openai_response = json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().to_string().replace("-", "").chars().take(10).collect::<String>()),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": self.current_model.read().as_str(),
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "refusal": null
                },
                "logprobs": null,
                "finish_reason": openai_finish_reason
            }],
            "usage": {
                "prompt_tokens": usage.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                "completion_tokens": usage.get("outputTokens").and_then(Value::as_u64).unwrap_or(0),
                "total_tokens": usage.get("totalTokens").and_then(Value::as_u64).unwrap_or(0),
                "prompt_tokens_details": {
                    "cached_tokens": 0,
                    "audio_tokens": 0
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 0,
                    "audio_tokens": 0,
                    "accepted_prediction_tokens": 0,
                    "rejected_prediction_tokens": 0
                }
            },
            "service_tier": "default",
            "system_fingerprint": format!("fp_{}", uuid::Uuid::new_v4().to_string().replace("-", "").chars().take(10).collect::<String>())
        });

        Ok(openai_response)
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn base_url(&self) -> String {
        self.base_url.read().clone()
    }

    fn name(&self) -> &str {
        "bedrock"
    }

    async fn before_request(&self, headers: &HeaderMap, body: &Bytes) -> Result<(), AppError> {
        // Extract and set the model from the request body before any other processing
        if let Ok(request_body) = serde_json::from_slice::<Value>(body) {
            if let Some(model) = request_body["model"].as_str() {
                debug!("Setting model from before_request: {}", model);
                *self.current_model.write() = model.to_string();
            }

            // Extract streaming flag from the request body
            let is_streaming = request_body
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            debug!(
                "Setting streaming flag from before_request: {}",
                is_streaming
            );
            *self.is_streaming.write() = is_streaming;

            // For each new request, generate a new system fingerprint
            let new_fingerprint = format!(
                "fp_{}",
                uuid::Uuid::new_v4()
                    .to_string()
                    .replace("-", "")
                    .chars()
                    .take(8)
                    .collect::<String>()
            );
            debug!("Generated new system fingerprint: {}", new_fingerprint);
            *self.system_fingerprint.write() = new_fingerprint;

            // Reset the first_chunk flag for a new request
            debug!("Resetting first_chunk flag for new request");
            *self.first_chunk.write() = true;
        }

        // Extract and set the region from the request headers
        if let Some(region) = headers.get("x-aws-region").and_then(|h| h.to_str().ok()) {
            debug!("Setting region from before_request: {}", region);
            *self.region.write() = region.to_string();
            *self.base_url.write() = format!("https://bedrock-runtime.{}.amazonaws.com", region);
        }

        Ok(())
    }

    fn transform_path(&self, _path: &str) -> String {
        let model = self.current_model.read();
        let is_streaming = *self.is_streaming.read();

        debug!(
            "Transforming path with model: {}, streaming: {}",
            *model, is_streaming
        );

        // Bedrock model identifiers may be inference-profile ARNs containing '/'
        // (e.g. `arn:aws:bedrock:us-east-1:123:inference-profile/us.anthropic...`).
        // A raw '/' would split the `/model/{id}/converse` path into extra
        // segments and corrupt the request, so encode it to keep the id within a
        // single segment. The same URL string is used for both SigV4 signing and
        // the outbound request, so the signature stays consistent. Foundation
        // model ids without a slash are left byte-for-byte unchanged.
        let model_path = model.replace('/', "%2F");

        if is_streaming {
            format!("/model/{}/converse-stream", model_path)
        } else {
            format!("/model/{}/converse", model_path)
        }
    }

    async fn prepare_request_body(&self, body: Bytes) -> Result<Bytes, AppError> {
        let request_body: Value = serde_json::from_slice(&body)?;
        let transformed_body = self.transform_request_body(request_body)?;
        Ok(Bytes::from(serde_json::to_vec(&transformed_body)?))
    }

    fn process_headers(&self, headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        let mut final_headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(headers);

        // Add standard headers
        final_headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Preserve AWS specific headers
        for (key, value) in headers {
            if key.as_str().starts_with("x-aws-") {
                final_headers.insert(key.clone(), value.clone());
            }
        }

        Ok(final_headers)
    }

    fn requires_signing(&self) -> bool {
        true
    }

    fn get_signing_credentials(&self, headers: &HeaderMap) -> Option<(String, String, String)> {
        let access_key = headers.get("x-aws-access-key-id")?.to_str().ok()?;
        let secret_key = headers.get("x-aws-secret-access-key")?.to_str().ok()?;
        let region = headers
            .get("x-aws-region")
            .and_then(|h| h.to_str().ok())
            .map(String::from)
            .unwrap_or_else(|| self.region.read().clone());

        Some((access_key.to_string(), secret_key.to_string(), region))
    }

    fn get_session_token(&self, headers: &HeaderMap) -> Option<String> {
        headers
            .get("x-aws-session-token")
            .and_then(|h| h.to_str().ok())
            .map(String::from)
    }

    fn get_signing_host(&self) -> String {
        let region = self.region.read().clone();
        format!("bedrock-runtime.{}.amazonaws.com", region)
    }

    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        // Extract AWS request ID if present
        let aws_request_id = response
            .headers()
            .get("x-amzn-RequestId")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        if let Some(request_id) = &aws_request_id {
            debug!("Extracted AWS Request ID: {}", request_id);
        }

        if response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("application/vnd.amazon.eventstream"))
        {
            debug!("Processing Bedrock event stream response");

            // Create transformed stream
            let provider = self.clone();
            let stream = response
                .into_body()
                .into_data_stream()
                .map(move |chunk| match chunk {
                    Ok(bytes) => match provider.transform_bedrock_chunk(bytes) {
                        Ok(transformed) => Ok(transformed),
                        Err(e) => {
                            error!("Error transforming chunk: {}", e);
                            Err(std::io::Error::other(e))
                        }
                    },
                    Err(e) => Err(std::io::Error::other(e)),
                });

            // Build response with transformed stream and all necessary headers
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                // SSE specific headers
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .header("transfer-encoding", "chunked")
                // CORS headers
                .header("access-control-allow-origin", "*")
                .header("access-control-allow-methods", "POST, OPTIONS")
                .header("access-control-allow-headers", "content-type, x-provider, x-aws-access-key-id, x-aws-secret-access-key, x-aws-region")
                .header("access-control-expose-headers", "*")
                // SSE specific headers for better client compatibility
                .header("x-accel-buffering", "no")
                .header("keep-alive", "timeout=600");

            // Add the request ID header if we have one
            if let Some(id) = aws_request_id {
                builder = builder.header("x-request-id", id);
            }

            Ok(builder.body(Body::from_stream(stream)).unwrap())
        } else {
            // For non-streaming responses, transform the body to OpenAI format
            debug!("Processing Bedrock non-streaming response");

            // Get response body using axum-compatible approach
            let (parts, body) = response.into_parts();
            let bytes = match futures_util::StreamExt::collect::<Vec<Result<Bytes, _>>>(
                body.into_data_stream(),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            {
                Ok(chunks) => {
                    let body_size = chunks.iter().map(|c| c.len()).sum();
                    let mut full_body = Vec::with_capacity(body_size);
                    for chunk in chunks {
                        full_body.extend_from_slice(&chunk);
                    }
                    Bytes::from(full_body)
                }
                Err(e) => {
                    return Err(AppError::HttpError(format!(
                        "Failed to collect body: {}",
                        e
                    )))
                }
            };

            // Parse the Bedrock response
            let bedrock_response: Value = serde_json::from_slice(&bytes)
                .map_err(|e| AppError::JsonParseError(e.to_string()))?;

            debug!("Original Bedrock response: {:?}", bedrock_response);

            // Transform to OpenAI format
            let openai_response = self.transform_bedrock_to_openai_format(bedrock_response)?;
            debug!("Transformed to OpenAI format: {:?}", openai_response);

            // Create new response with transformed body
            let transformed_body = serde_json::to_vec(&openai_response)
                .map_err(|e| AppError::JsonSerializeError(e.to_string()))?;

            // Build new response
            let mut builder = Response::builder()
                .status(parts.status)
                .header(http::header::CONTENT_TYPE, "application/json");

            // Copy the original headers
            for (name, value) in parts.headers {
                if let Some(name) = name {
                    // Skip the Content-Length header to avoid mismatch with the transformed body
                    if name != http::header::CONTENT_LENGTH {
                        builder = builder.header(name, value);
                    }
                }
            }

            // Add CORS headers
            builder = builder
                .header("access-control-allow-origin", "*")
                .header("access-control-allow-methods", "POST, OPTIONS")
                .header("access-control-allow-headers", "content-type, x-provider, x-aws-access-key-id, x-aws-secret-access-key, x-aws-region")
                .header("access-control-expose-headers", "*");

            // Add the request ID header if we have one
            if let Some(id) = aws_request_id {
                builder = builder.header("x-request-id", id);
            }

            Ok(builder
                .body(Body::from(transformed_body))
                .map_err(|e| AppError::HttpError(format!("Failed to build response: {}", e)))?)
        }
    }
}

// Add a metrics extractor for Bedrock
pub struct BedrockMetricsExtractor;

impl MetricsExtractor for BedrockMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!(
            "Extracting Bedrock metrics from response: {}",
            response_body
        );
        let mut metrics = ProviderMetrics::default();

        // Try extracting token information from Bedrock format first
        if let Some(usage) = response_body.get("usage") {
            debug!("Found usage data: {:?}", usage);

            // Check for Bedrock token format
            let input_tokens = usage
                .get("inputTokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let output_tokens = usage
                .get("outputTokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let total_tokens = usage
                .get("totalTokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            // If Bedrock format tokens weren't found, try OpenAI format
            let input_tokens = input_tokens.or_else(|| {
                usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
            });

            let output_tokens = output_tokens.or_else(|| {
                usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
            });

            let total_tokens = total_tokens.or_else(|| {
                usage
                    .get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
            });

            metrics.input_tokens = input_tokens;
            metrics.output_tokens = output_tokens;
            metrics.total_tokens = total_tokens;

            debug!(
                "Extracted tokens - input: {:?}, output: {:?}, total: {:?}",
                metrics.input_tokens, metrics.output_tokens, metrics.total_tokens
            );
        }

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found Bedrock model: {}", model);
            metrics.model = model.to_string();
        }

        // Extract request ID if present in the response body
        if let Some(request_id) = response_body.get("id").and_then(|v| v.as_str()) {
            debug!("Found Bedrock request ID in response body: {}", request_id);
            metrics.request_id = Some(request_id.to_string());
        } else if let Some(request_id) = response_body.get("requestId").and_then(|v| v.as_str()) {
            debug!("Found requestId in response body: {}", request_id);
            metrics.request_id = Some(request_id.to_string());
        }

        // Cost via the shared dual-rate pricing table.
        if let (Some(i), Some(o)) = (metrics.input_tokens, metrics.output_tokens) {
            let cost = crate::policy::pricing::estimate_cost(&metrics.model, i, o);
            if cost > 0.0 {
                metrics.cost = Some(cost);
            }
            debug!(
                "Calculated Bedrock cost: {:?} for model {}",
                metrics.cost, metrics.model
            );
        }

        debug!("Final extracted Bedrock metrics: {:?}", metrics);
        metrics
    }

    // Override with Bedrock-specific streaming metrics extraction
    fn try_extract_provider_specific_streaming_metrics(
        &self,
        chunk: &str,
    ) -> Option<ProviderMetrics> {
        debug!("Attempting Bedrock-specific streaming metrics extraction for chunk");

        // Try to parse the chunk as JSON
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // Check for indicators that this is a final message with metrics
            if json.get("usage").is_some() {
                debug!("Found usage in Bedrock streaming chunk, extracting complete metrics");
                return Some(self.extract_metrics(&json));
            }

            // For ongoing chunks, extract what we can
            let mut partial_metrics = ProviderMetrics::default();

            // Try to extract model information if available
            if let Some(model) = json.get("model").and_then(|m| m.as_str()) {
                partial_metrics.model = model.to_string();
            }

            // Try to extract request ID from various possible locations
            if let Some(request_id) = json.get("id").and_then(|v| v.as_str()) {
                debug!(
                    "Found request ID in Bedrock streaming chunk: {}",
                    request_id
                );
                partial_metrics.request_id = Some(request_id.to_string());
            } else if let Some(request_id) = json.get("requestId").and_then(|v| v.as_str()) {
                debug!("Found requestId in Bedrock streaming chunk: {}", request_id);
                partial_metrics.request_id = Some(request_id.to_string());
            }

            // Return partial metrics if we found anything useful
            if !partial_metrics.model.is_empty() || partial_metrics.request_id.is_some() {
                debug!("Returning partial Bedrock metrics from streaming chunk");
                return Some(partial_metrics);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_token_read_from_header_for_temporary_creds() {
        let p = BedrockProvider::new();
        // No header → None (long-lived credentials).
        assert_eq!(p.get_session_token(&HeaderMap::new()), None);
        // Present → returned (temporary STS credentials).
        let mut h = HeaderMap::new();
        h.insert("x-aws-session-token", "tok-abc123".parse().unwrap());
        assert_eq!(p.get_session_token(&h).as_deref(), Some("tok-abc123"));
    }

    #[test]
    fn transform_path_url_encodes_slash_in_arn_model() {
        // Inference-profile ARNs contain '/', which must be percent-encoded so it
        // stays within the single `/model/{id}/converse` path segment.
        let p = BedrockProvider::new();
        *p.current_model.write() =
            "arn:aws:bedrock:us-east-1:123:inference-profile/us.anthropic.claude".to_string();
        *p.is_streaming.write() = false;
        let path = p.transform_path("/v1/chat/completions");
        assert!(path.starts_with("/model/"));
        assert!(path.ends_with("/converse"));
        assert!(path.contains("%2F"), "slash must be encoded: {path}");
        assert!(
            !path.contains("profile/us"),
            "raw slash must not remain: {path}"
        );
    }

    #[test]
    fn transform_path_plain_model_is_unencoded_and_streams() {
        let p = BedrockProvider::new();
        *p.current_model.write() = "anthropic.claude-3-5-sonnet-20240620-v1:0".to_string();
        *p.is_streaming.write() = false;
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse"
        );
        *p.is_streaming.write() = true;
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1:0/converse-stream"
        );
    }

    #[test]
    fn extract_metrics_reads_bedrock_token_fields() {
        let body = json!({
            "model": "anthropic.claude-3-5-sonnet-20240620-v1:0",
            "usage": {"inputTokens": 80, "outputTokens": 20, "totalTokens": 100}
        });
        let m = BedrockMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(80));
        assert_eq!(m.output_tokens, Some(20));
        assert_eq!(m.total_tokens, Some(100));
    }

    #[test]
    fn extract_metrics_openai_shape_fallback() {
        let body = json!({
            "model": "x",
            "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
        });
        let m = BedrockMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(7));
        assert_eq!(m.output_tokens, Some(3));
        assert_eq!(m.total_tokens, Some(10));
    }
}
