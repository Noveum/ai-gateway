//! Generic OpenAI-compatible provider.
//!
//! Many providers expose an OpenAI-compatible Chat Completions API (same request
//! and response shape, Bearer auth). Rather than duplicate a near-identical
//! adapter per provider, this one generic provider serves all of them — the only
//! per-provider differences are the base URL and (optionally) a path rewrite.
//!
//! Cost is computed from the single-sourced [`crate::policy::pricing`] table by
//! model id, so every compatible provider gets accurate per-model pricing
//! without a bespoke cost function.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use tracing::{debug, error};

/// A provider that speaks the OpenAI Chat Completions wire format.
pub struct OpenAICompatibleProvider {
    name: &'static str,
    base_url: String,
    /// When true, strip a leading `/v1` from the request path before forwarding
    /// (for providers whose base URL already includes the version segment, e.g.
    /// Gemini's `/v1beta/openai` compatibility endpoint).
    strip_v1_prefix: bool,
}

impl OpenAICompatibleProvider {
    pub fn new(name: &'static str, base_url: impl Into<String>, strip_v1_prefix: bool) -> Self {
        Self {
            name,
            base_url: base_url.into(),
            strip_v1_prefix,
        }
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        self.name
    }

    fn transform_path(&self, path: &str) -> String {
        if self.strip_v1_prefix {
            // `/v1/chat/completions` -> `/chat/completions`
            if let Some(rest) = path.strip_prefix("/v1") {
                return rest.to_string();
            }
        }
        path.to_string()
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing {} request headers", self.name);
        let mut headers = HeaderMap::new();
        log_tracking_headers(original_headers);

        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        if let Some(auth) = original_headers
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        {
            // Validate the Bearer token shape locally so malformed credentials
            // fail fast as InvalidHeader rather than being forwarded upstream
            // (where they'd surface as an opaque 401 to the caller).
            if !auth.starts_with("Bearer ") {
                error!(
                    "Invalid authorization format for {} request - must start with 'Bearer '",
                    self.name
                );
                return Err(AppError::InvalidHeader);
            }
            if auth.len() <= 7 {
                error!("Empty Bearer token for {} request", self.name);
                return Err(AppError::InvalidHeader);
            }
            headers.insert(
                http::header::AUTHORIZATION,
                http::header::HeaderValue::from_str(auth).map_err(|_| {
                    error!("Failed to process {} authorization header", self.name);
                    AppError::InvalidHeader
                })?,
            );
        } else {
            error!("No authorization header found for {} request", self.name);
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

/// Metrics extractor for any OpenAI-compatible response. Reads the standard
/// `usage` object and computes cost from the shared pricing table by model id.
pub struct OpenAICompatibleMetricsExtractor;

impl MetricsExtractor for OpenAICompatibleMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        let mut metrics = ProviderMetrics::default();

        if let Some(model) = response_body.get("model").and_then(|v| v.as_str()) {
            metrics.model = model.to_string();
        }

        if let Some(usage) = response_body.get("usage") {
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
        }

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

    #[test]
    fn strip_v1_prefix_rewrites_path() {
        let p = OpenAICompatibleProvider::new("gemini", "https://x/v1beta/openai", true);
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/chat/completions"
        );
        assert_eq!(p.transform_path("/v1/embeddings"), "/embeddings");
    }

    #[test]
    fn no_strip_keeps_path() {
        let p = OpenAICompatibleProvider::new("deepseek", "https://api.deepseek.com", false);
        assert_eq!(
            p.transform_path("/v1/chat/completions"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn name_and_base() {
        let p = OpenAICompatibleProvider::new("xai", "https://api.x.ai", false);
        assert_eq!(p.name(), "xai");
        assert_eq!(p.base_url(), "https://api.x.ai");
    }

    #[test]
    fn requires_authorization() {
        let p = OpenAICompatibleProvider::new("deepseek", "https://api.deepseek.com", false);
        assert!(matches!(
            p.process_headers(&HeaderMap::new()),
            Err(AppError::MissingApiKey)
        ));
    }

    #[test]
    fn rejects_malformed_bearer() {
        let p = OpenAICompatibleProvider::new("deepseek", "https://api.deepseek.com", false);
        // Wrong scheme.
        let mut h = HeaderMap::new();
        h.insert("authorization", "Token abc".parse().unwrap());
        assert!(matches!(
            p.process_headers(&h),
            Err(AppError::InvalidHeader)
        ));
        // Empty token after "Bearer ".
        let mut h2 = HeaderMap::new();
        h2.insert("authorization", "Bearer ".parse().unwrap());
        assert!(matches!(
            p.process_headers(&h2),
            Err(AppError::InvalidHeader)
        ));
    }

    #[test]
    fn forwards_bearer_and_sets_json() {
        let p = OpenAICompatibleProvider::new("deepseek", "https://api.deepseek.com", false);
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer sk-x".parse().unwrap());
        let out = p.process_headers(&h).unwrap();
        assert_eq!(out.get(http::header::AUTHORIZATION).unwrap(), "Bearer sk-x");
        assert_eq!(
            out.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn extractor_reads_usage_and_costs_via_shared_table() {
        // gemini-2.5-flash via compat endpoint returns OpenAI-shaped usage.
        let body = serde_json::json!({
            "model": "gemini-2.5-flash",
            "usage": {"prompt_tokens": 1000, "completion_tokens": 500, "total_tokens": 1500}
        });
        let m = OpenAICompatibleMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(1000));
        assert_eq!(m.output_tokens, Some(500));
        let expected = (1000.0 / 1e6) * 0.30 + (500.0 / 1e6) * 2.50;
        assert!((m.cost.unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn extractor_unknown_model_no_cost() {
        let body = serde_json::json!({
            "model": "some-unknown-model",
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let m = OpenAICompatibleMetricsExtractor.extract_metrics(&body);
        assert!(m.cost.is_none());
    }
}
