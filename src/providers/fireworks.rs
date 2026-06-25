//! Fireworks AI provider adapter.
//!
//! Forwards to Fireworks' OpenAI-compatible API
//! (`https://api.fireworks.ai/inference/v1`; the incoming `/v1` prefix is
//! stripped since the base URL already carries it). Token usage comes from the
//! standard `usage` object and cost is priced via the shared table.

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

/// Provider adapter for Fireworks AI (`x-provider: fireworks`). Base URL
/// `https://api.fireworks.ai/inference/v1`; OpenAI-compatible wire format.
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

        // Cost via the shared dual-rate pricing table (Fireworks models are in it).
        if let (Some(i), Some(o)) = (metrics.input_tokens, metrics.output_tokens) {
            let cost = crate::policy::pricing::estimate_cost(&metrics.model, i, o);
            if cost > 0.0 {
                metrics.cost = Some(cost);
            }
        }

        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_url_includes_v1_and_path_is_stripped() {
        let p = FireworksProvider::new();
        assert_eq!(p.base_url(), "https://api.fireworks.ai/inference/v1");
        assert_eq!(p.name(), "fireworks");
        // base already carries /v1, so the incoming /v1 prefix is stripped.
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/chat/completions"
        );
    }

    #[test]
    fn extract_metrics_reads_usage_and_costs() {
        // Regression: Fireworks cost used to always be None despite the model
        // being in the pricing table.
        let body = json!({
            "id": "abc",
            "model": "accounts/fireworks/models/llama-v3p3-70b-instruct",
            "usage": {"prompt_tokens": 1000, "completion_tokens": 1000, "total_tokens": 2000}
        });
        let m = FireworksMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(1000));
        assert_eq!(m.output_tokens, Some(1000));
        assert_eq!(m.request_id.as_deref(), Some("abc"));
        // llama-v3p3-70b-instruct: 0.90 in / 0.90 out per 1M
        let expected = (1000.0 / 1e6) * 0.90 + (1000.0 / 1e6) * 0.90;
        assert!(m.cost.is_some(), "Fireworks cost must be computed");
        assert!((m.cost.unwrap() - expected).abs() < 1e-9);
    }
}
