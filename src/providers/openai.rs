//! OpenAI provider adapter.
//!
//! Forwards Chat Completions requests to `https://api.openai.com` unchanged
//! (the gateway speaks the OpenAI wire format natively) and extracts token usage
//! + cost from the standard `usage` object via the shared pricing table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, error};

/// Provider adapter for OpenAI (`x-provider: openai`). Base URL
/// `https://api.openai.com`; passes the Bearer token through unchanged.
pub struct OpenAIProvider {
    base_url: String,
}

impl Default for OpenAIProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAIProvider {
    pub fn new() -> Self {
        // `OPENAI_BASE_URL` override (standard OpenAI SDK convention) lets the
        // gateway target a compatible upstream — chiefly a local mock in the
        // hermetic E2E, or a self-hosted compatible endpoint.
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        Self { base_url }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing OpenAI request headers");
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
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            debug!("Using provided authorization header");
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process authorization header");
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for OpenAI request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

// OpenAI-specific metrics extractor
pub struct OpenAIMetricsExtractor;

impl MetricsExtractor for OpenAIMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        debug!("Extracting OpenAI metrics from response: {}", response_body);
        let mut metrics = ProviderMetrics::default();

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

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            debug!("Found model: {}", model);
            metrics.model = model.to_string();
        }

        // Cost via the single-sourced, dual-rate pricing table (input and output
        // priced separately). 0.0 (unknown model) is left as None.
        if let (Some(i), Some(o)) = (metrics.input_tokens, metrics.output_tokens) {
            let cost = crate::policy::pricing::estimate_cost(&metrics.model, i, o);
            if cost > 0.0 {
                metrics.cost = Some(cost);
            }
            debug!(
                "Calculated cost: {:?} for model {}",
                metrics.cost, metrics.model
            );
        }

        debug!("Final extracted metrics: {:?}", metrics);
        metrics
    }

    // Override with OpenAI-specific streaming metrics extraction
    fn try_extract_provider_specific_streaming_metrics(
        &self,
        chunk: &str,
    ) -> Option<ProviderMetrics> {
        debug!(
            "Attempting to extract metrics from OpenAI streaming chunk: {}",
            chunk
        );
        if let Ok(json) = serde_json::from_str::<Value>(chunk) {
            // If we have usage data, extract full metrics
            if json.get("usage").is_some() {
                debug!("Found usage in OpenAI streaming chunk, extracting metrics");
                return Some(self.extract_metrics(&json));
            }

            // For OpenAI streaming, extract what we can even if usage is missing
            // This will handle the common case where OpenAI omits token counts in streaming
            let model = json
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();

            if model.contains("gpt")
                || json.get("object").and_then(|o| o.as_str()).unwrap_or("")
                    == "chat.completion.chunk"
            {
                debug!("OpenAI streaming response detected without usage data, creating partial metrics");
                return Some(ProviderMetrics {
                    model,
                    provider_latency: Duration::from_millis(0), // We can't determine this from chunks
                    // Leave token counts and cost as None
                    ..Default::default()
                });
            }
        }
        debug!("No usage data found in OpenAI streaming chunk");
        None
    }
}

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
        let p = OpenAIProvider::new();
        assert_eq!(p.base_url(), "https://api.openai.com");
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn default_transform_path_is_identity() {
        let p = OpenAIProvider::new();
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn process_headers_forwards_auth_and_sets_json() {
        let p = OpenAIProvider::new();
        let out = p.process_headers(&hdr(Some("Bearer sk-test"))).unwrap();
        assert_eq!(
            out.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-test"
        );
        assert_eq!(
            out.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn process_headers_missing_auth_errors() {
        let p = OpenAIProvider::new();
        assert!(matches!(
            p.process_headers(&hdr(None)),
            Err(AppError::MissingApiKey)
        ));
    }

    #[test]
    fn extract_metrics_reads_usage_and_costs() {
        let body = json!({
            "model": "gpt-4o-mini",
            "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500}
        });
        let m = OpenAIMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(1000));
        assert_eq!(m.output_tokens, Some(500));
        assert_eq!(m.total_tokens, Some(1500));
        assert_eq!(m.model, "gpt-4o-mini");
        // gpt-4o-mini: 0.15 in / 0.60 out per 1M (dual-rate)
        let expected = (1000.0 / 1e6) * 0.15 + (500.0 / 1e6) * 0.60;
        assert!((m.cost.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn extract_metrics_unknown_model_no_cost() {
        let body = json!({
            "model": "made-up-model",
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        assert!(OpenAIMetricsExtractor.extract_metrics(&body).cost.is_none());
    }

    #[test]
    fn streaming_chunk_with_usage_is_extracted() {
        let chunk = r#"{"model":"gpt-4o","usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
        let m = OpenAIMetricsExtractor
            .try_extract_provider_specific_streaming_metrics(chunk)
            .expect("usage chunk should yield metrics");
        assert_eq!(m.total_tokens, Some(5));
    }
}
