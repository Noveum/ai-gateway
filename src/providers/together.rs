//! Together AI provider adapter.
//!
//! Forwards to Together's OpenAI-compatible API (`https://api.together.xyz`) and
//! reuses the OpenAI metrics extractor (standard `usage` object), pricing via the
//! shared table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderValue, Response},
};
use serde_json::Value;
use tracing::{debug, error};
use uuid::Uuid;

/// Provider adapter for Together AI (`x-provider: together`). Base URL
/// `https://api.together.xyz`; OpenAI-compatible wire format.
pub struct TogetherProvider {
    base_url: String,
}

impl Default for TogetherProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TogetherProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.together.xyz".to_string(),
        }
    }
}

#[async_trait]
impl Provider for TogetherProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "together"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Together request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Add content type
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        {
            // Validate token format
            if !auth.starts_with("Bearer ") {
                error!(
                    "Invalid authorization format for Together request - must start with 'Bearer '"
                );
                return Err(AppError::InvalidHeader);
            }

            // Validate token is not empty after "Bearer "
            if auth.len() <= 7 {
                error!("Empty Bearer token in Together authorization header");
                return Err(AppError::InvalidHeader);
            }

            debug!("Using provided authorization header for Together");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Invalid characters in Together authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("Missing Bearer token in Authorization header for Together request");
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

        // For streaming responses
        if is_streaming {
            // Check if we already have a request ID in headers
            let has_request_id = parts.headers.get("x-request-id").is_some();

            // If no request ID, generate one
            if !has_request_id {
                let generated_id = format!("req_{}", Uuid::new_v4().simple());
                debug!(
                    "Generated request ID for Together streaming response: {}",
                    generated_id
                );

                if let Ok(header_value) = HeaderValue::from_str(&generated_id) {
                    parts.headers.insert("x-request-id", header_value);
                }
            }

            // Return streaming response with updated headers
            return Ok(Response::from_parts(parts, body));
        }

        // For regular responses
        let bytes = to_bytes(body, usize::MAX).await?;

        // Check if we already have a request ID
        let has_request_id = parts.headers.get("x-request-id").is_some();

        // If no request ID in headers, try to extract from response body
        if !has_request_id {
            if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
                // Try to extract ID from JSON response
                let body_request_id = json
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| id.to_string());

                if let Some(id) = body_request_id {
                    debug!(
                        "Adding Together request ID from body to response headers: {}",
                        id
                    );
                    if let Ok(header_value) = HeaderValue::from_str(&id) {
                        parts.headers.insert("x-request-id", header_value);
                    }
                } else {
                    // If no ID found in body either, generate one
                    let generated_id = format!("req_{}", Uuid::new_v4().simple());
                    debug!(
                        "Generated request ID for Together response: {}",
                        generated_id
                    );

                    if let Ok(header_value) = HeaderValue::from_str(&generated_id) {
                        parts.headers.insert("x-request-id", header_value);
                    }
                }

                // Return response with JSON body and updated headers
                return Ok(Response::from_parts(parts, Body::from(bytes)));
            }
        }

        // If we couldn't parse JSON or already have a request ID, return with original body
        Ok(Response::from_parts(parts, Body::from(bytes)))
    }
}
