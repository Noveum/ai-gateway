use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{HeaderMap, Response},
};
use tracing::{debug, error};

pub struct FireworksProvider {
    base_url: String,
}

impl Default for FireworksProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FireworksProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
        }
    }
}

#[async_trait]
impl Provider for FireworksProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "fireworks"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Fireworks request headers");
        let mut headers = HeaderMap::new();

        // Log tracking headers for observability
        log_tracking_headers(original_headers);

        // Add standard headers
        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        headers.insert(
            http::header::ACCEPT,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Process authentication
        if let Some(auth) = original_headers
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        {
            // Validate token is not empty
            if auth.trim().is_empty() {
                error!("Empty authorization token provided for Fireworks");
                return Err(AppError::InvalidHeader);
            }

            // Validate token format
            if !auth.starts_with("Bearer ") {
                error!("Invalid authorization format for Fireworks - must start with 'Bearer'");
                return Err(AppError::InvalidHeader);
            }

            // Validate token is not just "Bearer "
            if auth.len() <= 7 {
                error!("Empty Bearer token in Fireworks authorization header");
                return Err(AppError::InvalidHeader);
            }

            debug!("Using provided authorization header for Fireworks");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Invalid characters in Fireworks authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("Missing 'Authorization' header for Fireworks API request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }

    fn transform_path(&self, path: &str) -> String {
        // The incoming path is /v1/chat/completions
        // We want to strip the /v1 prefix since it's already in the base_url
        if path.starts_with("/v1/") {
            path.trim_start_matches("/v1").to_string()
        } else {
            path.to_string()
        }
    }

    async fn process_response(&self, response: Response<Body>) -> Result<Response<Body>, AppError> {
        let (mut parts, body) = response.into_parts();

        // Extract the Fireworks request ID from the response headers if present
        if let Some(id) = parts.headers.get("x-request-id").cloned() {
            debug!("Found Fireworks x-request-id header: {:?}", id);
            // Ensure this header is passed through to the client for validation
            parts.headers.insert("x-request-id", id);
        } else {
            debug!("No x-request-id found in Fireworks response headers");
        }

        Ok(Response::from_parts(parts, body))
    }
}

// Fireworks-specific metrics extractor
pub struct FireworksMetricsExtractor;

impl MetricsExtractor for FireworksMetricsExtractor {
    fn extract_metrics(&self, response_body: &serde_json::Value) -> ProviderMetrics {
        debug!(
            "Extracting Fireworks metrics from response: {}",
            response_body
        );
        let mut metrics = ProviderMetrics::default();

        // Extract token information from usage field (OpenAI compatible format)
        if let Some(usage) = response_body.get("usage") {
            debug!("Found usage data: {:?}", usage);
            metrics.input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            metrics.output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            metrics.total_tokens = usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            debug!(
                "Extracted tokens - input: {:?}, output: {:?}, total: {:?}",
                metrics.input_tokens, metrics.output_tokens, metrics.total_tokens
            );
        }

        // Extract model information
        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found model: {}", model);
            metrics.model = model.to_string();
        }

        // Extract request ID
        if let Some(id) = response_body.get("id").and_then(|v| v.as_str()) {
            debug!("Found request ID: {}", id);
            metrics.request_id = Some(id.to_string());
        }

        metrics
    }
}
