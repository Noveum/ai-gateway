//! Noveum trace exporter.
//!
//! Ships per-request [`RequestMetrics`] to the Noveum platform's trace-ingest
//! endpoint (`POST /v1/traces`) as an OpenTelemetry-compatible log, reusing the
//! org/project/user context the gateway already captures from request headers.
//! This makes gateway traffic appear in the same Noveum traces, dashboards, and
//! (once policy decisions are threaded through) the Nova Guard violation feed as
//! SDK traffic — one unified view across both enforcement surfaces.

use async_trait::async_trait;
use reqwest::{header, Client};
use std::time::Duration;
use tracing::{debug, warn};

use crate::telemetry::metrics::MetricsExporter;
use crate::telemetry::RequestMetrics;

/// Posts metrics to a Noveum-compatible trace-ingest endpoint.
pub struct NoveumTraceExporter {
    client: Client,
    /// Full ingest URL, e.g. `https://api.noveum.ai/api/v1/traces`.
    ingest_url: String,
    api_key: String,
}

impl NoveumTraceExporter {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        let base = endpoint.into();
        let base = base.trim_end_matches('/');
        let ingest_url = format!("{base}/v1/traces");
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            client,
            ingest_url,
            api_key: api_key.into(),
        }
    }

    /// Build from environment when `ENABLE_NOVEUM_TRACES=true` and both
    /// `NOVEUM_ENDPOINT` and `NOVEUM_API_KEY` are set.
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("ENABLE_NOVEUM_TRACES")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let endpoint = std::env::var("NOVEUM_ENDPOINT").ok()?;
        let api_key = std::env::var("NOVEUM_API_KEY").ok()?;
        if endpoint.is_empty() || api_key.is_empty() {
            return None;
        }
        Some(Self::new(endpoint, api_key))
    }
}

#[async_trait]
impl MetricsExporter for NoveumTraceExporter {
    async fn export_metrics(
        &self,
        metrics: RequestMetrics,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let payload = metrics.to_otel_log();
        let resp = self
            .client
            .post(&self.ingest_url)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.api_key))
            .json(&payload)
            .send()
            .await?;

        if resp.status().is_success() {
            debug!("exported trace to Noveum ({})", resp.status());
            Ok(())
        } else {
            let status = resp.status();
            warn!("Noveum trace ingest returned {}", status);
            Err(format!("noveum trace ingest status {status}").into())
        }
    }

    fn name(&self) -> &str {
        "noveum_trace"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header as match_header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_metrics() -> RequestMetrics {
        RequestMetrics {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cost: Some(0.001),
            project_id: Some("proj_1".into()),
            org_id: Some("org_1".into()),
            status_code: 200,
            ..Default::default()
        }
    }

    #[test]
    fn builds_ingest_url() {
        let e = NoveumTraceExporter::new("https://api.noveum.ai/api/", "k");
        assert_eq!(e.ingest_url, "https://api.noveum.ai/api/v1/traces");
        assert_eq!(e.name(), "noveum_trace");
    }

    #[tokio::test]
    async fn posts_otel_log_with_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .and(match_header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let e = NoveumTraceExporter::new(server.uri(), "secret");
        e.export_metrics(sample_metrics()).await.unwrap();
    }

    #[tokio::test]
    async fn error_status_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let e = NoveumTraceExporter::new(server.uri(), "secret");
        assert!(e.export_metrics(sample_metrics()).await.is_err());
    }

    #[test]
    fn from_env_disabled_by_default() {
        std::env::remove_var("ENABLE_NOVEUM_TRACES");
        assert!(NoveumTraceExporter::from_env().is_none());
    }
}
