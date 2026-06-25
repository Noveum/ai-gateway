//! Cohere provider (v2 chat).
//!
//! Cohere's v2 chat API lives at `/v2/chat`. Authentication is a Bearer token.
//! This adapter handles auth + path mapping and extracts usage from Cohere's
//! `usage` object (`billed_units` / `tokens`). Full OpenAI<->Cohere body
//! translation is intentionally out of scope here; callers send Cohere-native
//! bodies. Cost is computed from the shared pricing table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use tracing::{debug, error};

pub struct CohereProvider {
    base_url: String,
}

impl CohereProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://api.cohere.com".to_string(),
        }
    }
}

impl Default for CohereProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CohereProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "cohere"
    }

    /// Map an OpenAI-style chat path to Cohere's v2 chat endpoint; pass other
    /// paths through unchanged.
    fn transform_path(&self, path: &str) -> String {
        if path.contains("/chat/completions") {
            "/v2/chat".to_string()
        } else {
            path.to_string()
        }
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Cohere request headers");
        let mut headers = HeaderMap::new();
        log_tracking_headers(original_headers);

        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process Cohere authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for Cohere request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

pub struct CohereMetricsExtractor;

impl MetricsExtractor for CohereMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        let mut metrics = ProviderMetrics::default();

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            metrics.model = model.to_string();
        }

        // Cohere v2: usage.tokens.{input_tokens,output_tokens} or billed_units.
        if let Some(usage) = response_body.get("usage") {
            let tokens = usage.get("tokens").or_else(|| usage.get("billed_units"));
            if let Some(t) = tokens {
                metrics.input_tokens = t
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                metrics.output_tokens = t
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                metrics.total_tokens = match (metrics.input_tokens, metrics.output_tokens) {
                    (Some(i), Some(o)) => Some(i + o),
                    _ => None,
                };
            }
        }

        if let (Some(i), Some(o)) = (metrics.input_tokens, metrics.output_tokens) {
            metrics.cost = Some(crate::policy::pricing::estimate_cost(&metrics.model, i, o));
        }

        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_path_maps_to_v2_chat() {
        let p = CohereProvider::new();
        assert_eq!(p.transform_path("/v1/chat/completions"), "/v2/chat");
        assert_eq!(p.transform_path("/v1/embeddings"), "/v1/embeddings");
    }

    #[test]
    fn name_and_base() {
        let p = CohereProvider::new();
        assert_eq!(p.name(), "cohere");
        assert_eq!(p.base_url(), "https://api.cohere.com");
    }

    #[test]
    fn requires_auth() {
        let p = CohereProvider::new();
        assert!(matches!(
            p.process_headers(&HeaderMap::new()),
            Err(AppError::MissingApiKey)
        ));
    }

    #[test]
    fn extracts_usage_tokens() {
        let body = serde_json::json!({
            "model": "command-r-08-2024",
            "usage": {"tokens": {"input_tokens": 1000, "output_tokens": 200}}
        });
        let m = CohereMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(1000));
        assert_eq!(m.output_tokens, Some(200));
        assert_eq!(m.total_tokens, Some(1200));
        // command-r-08-2024: 0.15 in / 0.60 out per 1M
        let expected = (1000.0 / 1e6) * 0.15 + (200.0 / 1e6) * 0.60;
        assert!((m.cost.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn extracts_billed_units_fallback() {
        let body = serde_json::json!({
            "model": "command-r-plus-08-2024",
            "usage": {"billed_units": {"input_tokens": 10, "output_tokens": 5}}
        });
        let m = CohereMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(10));
        assert_eq!(m.output_tokens, Some(5));
    }
}
