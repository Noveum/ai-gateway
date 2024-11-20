use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use aws_event_stream_parser::{parse_message, Message};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Response, StatusCode},
};
use futures_util::StreamExt;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, error, warn};

/// Constants for default values
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MODEL: &str = "amazon.titan-text-premier-v1:0";
const DEFAULT_FALLBACK_MODEL: &str = "mistral.mistral-7b-instruct-v0:2";
const DEFAULT_MAX_TOKENS: u64 = 1000;
const DEFAULT_TEMPERATURE: f64 = 0.7;
const DEFAULT_TOP_P: f64 = 1.0;

/// BedrockProvider handles AWS Bedrock API integration
#[derive(Clone)]
pub struct BedrockProvider {
    base_url: Arc<RwLock<String>>,
    region: Arc<RwLock<String>>,
    current_model: Arc<RwLock<String>>,
}

impl BedrockProvider {
    pub fn new() -> Self {
        let region = DEFAULT_REGION.to_string();
        debug!("Initializing BedrockProvider with region: {}", region);

        Self {
            base_url: Arc::new(RwLock::new(format!(
                "https://bedrock-runtime.{}.amazonaws.com",
                region
            ))),
            region: Arc::new(RwLock::new(region)),
            current_model: Arc::new(RwLock::new(DEFAULT_MODEL.to_string())),
        }
    }

    fn get_model_name(&self, path: &str) -> String {
        path.split('/')
            .last()
            .map(ToString::to_string)
            .unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_string())
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
        json!({
            "id": "chatcmpl-bedrock",
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.current_model.read().as_str(),
            "choices": [{
                "index": 0,
                "delta": {
                    "content": delta
                },
                "finish_reason": null
            }]
        })
    }

    fn create_final_response(&self, usage: &Value) -> Value {
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
            "usage": usage
        })
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
        }

        // Extract and set the region from the request headers
        if let Some(region) = headers.get("x-aws-region").and_then(|h| h.to_str().ok()) {
            debug!("Setting region from before_request: {}", region);
            *self.region.write() = region.to_string();
            *self.base_url.write() = format!("https://bedrock-runtime.{}.amazonaws.com", region);
        }

        Ok(())
    }

    fn transform_path(&self, path: &str) -> String {
        let model = self.current_model.read();
        debug!("Transforming path with model: {}", *model);
        format!("/model/{}/converse-stream", *model)
    }

    async fn prepare_request_body(&self, body: Bytes) -> Result<Bytes, AppError> {
        let request_body: Value = serde_json::from_slice(&body)?;
        let transformed_body = self.transform_request_body(request_body)?;
        Ok(Bytes::from(serde_json::to_vec(&transformed_body)?))
    }

    fn process_headers(&self, headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        let mut final_headers = HeaderMap::new();

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

    fn get_signing_host(&self) -> String {
        let region = self.region.read().clone();
        format!("bedrock-runtime.{}.amazonaws.com", region)
    }

    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        if response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map_or(false, |ct| {
                ct.contains("application/vnd.amazon.eventstream")
            })
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
                            Err(std::io::Error::new(std::io::ErrorKind::Other, e))
                        }
                    },
                    Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                });

            // Build response with transformed stream and all necessary headers
            Ok(Response::builder()
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
                .header("keep-alive", "timeout=600")
                .body(Body::from_stream(stream))
                .unwrap())
        } else {
            // For non-streaming responses, still add CORS headers
            let mut response = response;
            let headers = response.headers_mut();
            headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
            headers.insert(
                "access-control-allow-methods",
                HeaderValue::from_static("POST, OPTIONS"),
            );
            headers.insert("access-control-allow-headers",
                           HeaderValue::from_static("content-type, x-provider, x-aws-access-key-id, x-aws-secret-access-key, x-aws-region"));
            headers.insert(
                "access-control-expose-headers",
                HeaderValue::from_static("*"),
            );
            Ok(response)
        }
    }
}
