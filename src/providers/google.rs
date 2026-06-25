//! Google Gemini provider (Generative Language API).
//!
//! Gemini's REST API is at `generativelanguage.googleapis.com`, with the model
//! in the URL path (`/v1beta/models/{model}:generateContent`) and an API key
//! supplied via the `x-goog-api-key` header. This adapter maps an incoming
//! `Authorization: Bearer <key>` to `x-goog-api-key` and extracts usage from
//! `usageMetadata`. Callers send Gemini-native bodies; OpenAI<->Gemini body
//! translation is out of scope here. Cost is computed from the shared table.

use super::utils::log_tracking_headers;
use super::Provider;
use crate::error::AppError;
use crate::telemetry::provider_metrics::{MetricsExtractor, ProviderMetrics};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use tracing::{debug, error};

pub struct GoogleProvider {
    base_url: String,
}

impl GoogleProvider {
    pub fn new() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com".to_string(),
        }
    }
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GoogleProvider {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn name(&self) -> &str {
        "google"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        debug!("Processing Google Gemini request headers");
        let mut headers = HeaderMap::new();
        log_tracking_headers(original_headers);

        headers.insert(
            http::header::CONTENT_TYPE,
            http::header::HeaderValue::from_static("application/json"),
        );

        // Prefer an explicit x-goog-api-key; otherwise map a Bearer token.
        if let Some(key) = original_headers
            .get("x-goog-api-key")
            .and_then(|h| h.to_str().ok())
        {
            headers.insert(
                "x-goog-api-key",
                http::header::HeaderValue::from_str(key).map_err(|_| AppError::InvalidHeader)?,
            );
        } else if let Some(auth) = original_headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
        {
            let key = auth.strip_prefix("Bearer ").unwrap_or(auth);
            headers.insert(
                "x-goog-api-key",
                http::header::HeaderValue::from_str(key).map_err(|_| AppError::InvalidHeader)?,
            );
        } else {
            error!("No API key found for Google Gemini request");
            return Err(AppError::MissingApiKey);
        }

        Ok(headers)
    }
}

pub struct GoogleMetricsExtractor;

impl MetricsExtractor for GoogleMetricsExtractor {
    fn extract_metrics(&self, response_body: &Value) -> ProviderMetrics {
        let mut metrics = ProviderMetrics::default();

        if let Some(model) = response_body
            .get("modelVersion")
            .or_else(|| response_body.get("model"))
            .and_then(|v| v.as_str())
        {
            metrics.model = model.to_string();
        }

        if let Some(usage) = response_body.get("usageMetadata") {
            metrics.input_tokens = usage
                .get("promptTokenCount")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            metrics.output_tokens = usage
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            metrics.total_tokens = usage
                .get("totalTokenCount")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
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
    fn name_and_base() {
        let p = GoogleProvider::new();
        assert_eq!(p.name(), "google");
        assert!(p.base_url().contains("generativelanguage"));
    }

    #[test]
    fn maps_bearer_to_goog_api_key() {
        let p = GoogleProvider::new();
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer AIzaSyXYZ".parse().unwrap());
        let out = p.process_headers(&h).unwrap();
        assert_eq!(out.get("x-goog-api-key").unwrap(), "AIzaSyXYZ");
    }

    #[test]
    fn uses_explicit_goog_api_key() {
        let p = GoogleProvider::new();
        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", "direct-key".parse().unwrap());
        let out = p.process_headers(&h).unwrap();
        assert_eq!(out.get("x-goog-api-key").unwrap(), "direct-key");
    }

    #[test]
    fn requires_some_key() {
        let p = GoogleProvider::new();
        assert!(matches!(
            p.process_headers(&HeaderMap::new()),
            Err(AppError::MissingApiKey)
        ));
    }

    #[test]
    fn extracts_usage_metadata() {
        let body = serde_json::json!({
            "modelVersion": "gemini-2.5-flash",
            "usageMetadata": {"promptTokenCount": 1000, "candidatesTokenCount": 500, "totalTokenCount": 1500}
        });
        let m = GoogleMetricsExtractor.extract_metrics(&body);
        assert_eq!(m.input_tokens, Some(1000));
        assert_eq!(m.output_tokens, Some(500));
        assert_eq!(m.total_tokens, Some(1500));
        // gemini-2.5-flash: 0.30 in / 2.50 out per 1M
        let expected = (1000.0 / 1e6) * 0.30 + (500.0 / 1e6) * 2.50;
        assert!((m.cost.unwrap() - expected).abs() < 1e-12);
    }
}
