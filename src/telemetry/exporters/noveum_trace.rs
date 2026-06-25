//! Noveum trace exporter.
//!
//! Ships per-request [`RequestMetrics`] to the Noveum platform's trace-ingest
//! endpoint as a **span-based trace batch** matching the `noveum-trace` Python
//! SDK wire format: `POST {endpoint}/v1/traces` with a `{ "traces": [...],
//! "timestamp": ... }` body, where each trace carries one span with `llm.*`
//! attributes. This makes gateway traffic appear in the same Noveum project
//! view, dashboards, and trace explorer as SDK traffic — one unified surface.
//!
//! Project attribution uses the per-request `x-project-id` (captured into
//! [`RequestMetrics::project_id`]); when absent, a `NOVEUM_PROJECT` default is
//! used. The ingest API requires a project, so requests without either are
//! skipped (logged at debug) rather than sent and rejected.

use async_trait::async_trait;
use reqwest::{header, Client};
use serde_json::json;
use std::time::Duration;
use tracing::{debug, warn};

use crate::telemetry::metrics::MetricsExporter;
use crate::telemetry::RequestMetrics;

/// Posts traces to a Noveum-compatible trace-ingest endpoint.
pub struct NoveumTraceExporter {
    client: Client,
    /// Full batch ingest URL, e.g. `https://api.noveum.ai/api/v1/traces`.
    ingest_url: String,
    api_key: String,
    /// Fallback project id when a request carries no `x-project-id`.
    default_project: Option<String>,
    /// Environment tag applied to exported traces (e.g. `production`).
    environment: String,
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
            default_project: None,
            environment: "production".to_string(),
        }
    }

    /// Set the fallback project id used when a request has no `x-project-id`.
    pub fn with_default_project(mut self, project: Option<String>) -> Self {
        self.default_project = project.filter(|s| !s.is_empty());
        self
    }

    /// Set the environment tag applied to exported traces.
    pub fn with_environment(mut self, environment: impl Into<String>) -> Self {
        let env = environment.into();
        if !env.is_empty() {
            self.environment = env;
        }
        self
    }

    /// Build from environment when `ENABLE_NOVEUM_TRACES=true` and both
    /// `NOVEUM_ENDPOINT` and `NOVEUM_API_KEY` are set. A `NOVEUM_PROJECT` default
    /// and `NOVEUM_ENVIRONMENT` tag are picked up when present.
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
        let default_project = std::env::var("NOVEUM_PROJECT").ok();
        let environment =
            std::env::var("NOVEUM_ENVIRONMENT").unwrap_or_else(|_| "production".to_string());
        Some(
            Self::new(endpoint, api_key)
                .with_default_project(default_project)
                .with_environment(environment),
        )
    }

    /// Resolve the project id for a request: the per-request `project_id` wins,
    /// then the configured default. Returns `None` when neither is set.
    fn resolve_project<'a>(&'a self, metrics: &'a RequestMetrics) -> Option<&'a str> {
        metrics
            .project_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.default_project.as_deref())
    }
}

#[async_trait]
impl MetricsExporter for NoveumTraceExporter {
    async fn export_metrics(
        &self,
        metrics: RequestMetrics,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(project) = self.resolve_project(&metrics) else {
            debug!(
                "Noveum trace export skipped: no project (set the x-project-id header \
                 or NOVEUM_PROJECT)"
            );
            return Ok(());
        };

        let trace = metrics.to_noveum_trace(project, &self.environment);
        let payload = json!({
            "traces": [trace],
            "timestamp": chrono::Utc::now().timestamp(),
        });

        let resp = self
            .client
            .post(&self.ingest_url)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(
                header::USER_AGENT,
                concat!("noveum-ai-gateway/", env!("CARGO_PKG_VERSION")),
            )
            .json(&payload)
            .send()
            .await?;

        if resp.status().is_success() {
            debug!("exported trace to Noveum ({})", resp.status());
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(
                "Noveum trace ingest returned {} (body: {})",
                status,
                body.chars().take(300).collect::<String>()
            );
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
    async fn posts_sdk_trace_batch_with_auth() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .and(match_header("authorization", "Bearer secret"))
            // Assert the SDK-compatible envelope: traces[0].project + a span.
            .and(body_partial_json(json!({
                "traces": [{
                    "project": "proj_1",
                    "status": "ok",
                    "span_count": 1,
                    "sdk": { "name": "noveum-ai-gateway" },
                    "attributes": { "llm.model": "gpt-4o", "llm.provider": "openai" },
                }]
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let e = NoveumTraceExporter::new(server.uri(), "secret");
        e.export_metrics(sample_metrics()).await.unwrap();
    }

    #[test]
    fn serializes_full_trace_shape() {
        // The serialized trace must carry every field the ingest schema requires
        // plus the llm.* attributes the Noveum UI parses.
        let m = sample_metrics();
        let t = m.to_noveum_trace("proj_1", "production");
        for k in [
            "trace_id",
            "name",
            "start_time",
            "end_time",
            "duration_ms",
            "status",
            "span_count",
            "error_count",
            "project",
            "environment",
            "sdk",
            "metadata",
            "spans",
        ] {
            assert!(t.get(k).is_some(), "trace missing field {k}");
        }
        assert_eq!(t["project"], "proj_1");
        assert_eq!(t["sdk"]["name"], "noveum-ai-gateway");
        let span = &t["spans"][0];
        for k in [
            "span_id",
            "trace_id",
            "name",
            "start_time",
            "end_time",
            "duration_ms",
            "status",
            "attributes",
        ] {
            assert!(span.get(k).is_some(), "span missing field {k}");
        }
        let a = &span["attributes"];
        assert_eq!(a["llm.model"], "gpt-4o");
        assert_eq!(a["llm.usage.prompt_tokens"], 100);
        assert_eq!(a["llm.usage.completion_tokens"], 50);
        assert_eq!(a["llm.cost.total"], 0.001);
        assert_eq!(a["llm.cost.currency"], "USD");
    }

    #[test]
    fn provider_http_error_marks_trace_error() {
        // A proxied provider 4xx must surface as status=error with error_count>=1,
        // even when the gateway's own error counters were not incremented.
        let mut m = sample_metrics();
        m.status_code = 404;
        m.provider_status_code = 404;
        let t = m.to_noveum_trace("proj_1", "production");
        assert_eq!(t["status"], "error");
        assert!(t["error_count"].as_u64().unwrap() >= 1);
        assert_eq!(t["spans"][0]["status"], "error");
    }

    #[tokio::test]
    async fn skips_export_without_project() {
        // No project id and no default -> must NOT POST anything.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut m = sample_metrics();
        m.project_id = None;
        let e = NoveumTraceExporter::new(server.uri(), "secret");
        e.export_metrics(m).await.unwrap(); // Ok, but no request sent.
    }

    #[tokio::test]
    async fn uses_default_project_when_header_absent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .and(wiremock::matchers::body_partial_json(json!({
                "traces": [{ "project": "fallback_proj" }]
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut m = sample_metrics();
        m.project_id = None;
        let e = NoveumTraceExporter::new(server.uri(), "secret")
            .with_default_project(Some("fallback_proj".into()));
        e.export_metrics(m).await.unwrap();
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
