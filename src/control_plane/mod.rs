//! Noveum control-plane client.
//!
//! A typed HTTP client for the gateway's interactions with the Noveum backend:
//! * **Budget reservation** — `POST /v1/projects/:id/budget/reserve` and
//!   `/reconcile`. This is the atomic (Redis-Lua-backed) primitive that gives
//!   the gateway the same hard-cap concurrency guarantee as the SDK strict mode.
//! * **Policy distribution** — `GET /v1/projects/:id/policies` with
//!   ETag-conditional polling, used by the background refresh task to hot-swap
//!   the engine's policy set.
//!
//! The client is resilient: transient failures are retried with exponential
//! backoff (`backon`), and callers decide fail-open vs fail-closed on terminal
//! failure. When no control plane is configured the gateway runs standalone with
//! local policies only.

use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::policy::config::PolicyBundle;

/// Configuration for the control-plane client.
#[derive(Debug, Clone)]
pub struct ControlPlaneConfig {
    pub base_url: String,
    pub api_key: String,
    pub request_timeout: Duration,
    pub max_retries: usize,
}

impl ControlPlaneConfig {
    /// Build from environment: `NOVEUM_ENDPOINT` + `NOVEUM_API_KEY`. Returns
    /// `None` when not configured (standalone mode).
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("NOVEUM_ENDPOINT").ok()?;
        let api_key = std::env::var("NOVEUM_API_KEY").ok()?;
        if base_url.is_empty() || api_key.is_empty() {
            return None;
        }
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            request_timeout: Duration::from_secs(5),
            max_retries: 3,
        })
    }
}

#[derive(Debug, Serialize)]
struct ReserveRequest<'a> {
    reserved_usd: f64,
    model: &'a str,
    request_id: &'a str,
}

/// Result of a budget reservation attempt.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReserveResponse {
    pub admit: bool,
    #[serde(default)]
    pub reservation_id: Option<String>,
    #[serde(default)]
    pub headroom_usd: f64,
    #[serde(default)]
    pub current_spend_usd: f64,
}

#[derive(Debug, Serialize)]
struct ReconcileRequest<'a> {
    reservation_id: &'a str,
    actual_usd: f64,
}

/// Outcome of fetching policies with a conditional GET.
#[derive(Debug)]
pub enum PolicyFetch {
    /// Server returned a new bundle (with its ETag, if any).
    Updated {
        bundle: PolicyBundle,
        etag: Option<String>,
    },
    /// `304 Not Modified` — the cached bundle is current.
    NotModified,
}

/// Errors the client surfaces; `Transient` is retried, others are terminal.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("transient control-plane error: {0}")]
    Transient(String),
    #[error("control-plane auth failed")]
    Unauthorized,
    #[error("control-plane request failed: {0}")]
    Fatal(String),
}

impl ControlPlaneError {
    fn is_transient(&self) -> bool {
        matches!(self, ControlPlaneError::Transient(_))
    }
}

#[derive(Clone)]
pub struct ControlPlaneClient {
    http: Client,
    config: ControlPlaneConfig,
}

impl ControlPlaneClient {
    pub fn new(config: ControlPlaneConfig) -> Self {
        let http = Client::builder()
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();
        Self { http, config }
    }

    fn backoff(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(50))
            .with_max_delay(Duration::from_secs(2))
            .with_max_times(self.config.max_retries)
            .with_jitter()
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.api_key)
    }

    /// Atomically reserve budget before an LLM call. Retries transient errors.
    pub async fn reserve(
        &self,
        project_id: &str,
        reserved_usd: f64,
        model: &str,
        request_id: &str,
    ) -> Result<ReserveResponse, ControlPlaneError> {
        let url = format!(
            "{}/v1/projects/{}/budget/reserve",
            self.config.base_url, project_id
        );
        let body = ReserveRequest {
            reserved_usd,
            model,
            request_id,
        };

        let call = || async {
            let resp = self
                .http
                .post(&url)
                .header(header::AUTHORIZATION, self.auth_header())
                .json(&body)
                .send()
                .await
                .map_err(|e| ControlPlaneError::Transient(e.to_string()))?;

            match resp.status() {
                StatusCode::OK => resp
                    .json::<ReserveResponse>()
                    .await
                    .map_err(|e| ControlPlaneError::Fatal(format!("bad reserve body: {e}"))),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    Err(ControlPlaneError::Unauthorized)
                }
                s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
                    Err(ControlPlaneError::Transient(format!("status {s}")))
                }
                s => Err(ControlPlaneError::Fatal(format!("status {s}"))),
            }
        };

        call.retry(self.backoff())
            .when(|e: &ControlPlaneError| e.is_transient())
            .notify(|e, d| warn!(?d, error=%e, "control-plane reserve retry"))
            .await
    }

    /// Settle a reservation after the call completes (best-effort; logs on error).
    pub async fn reconcile(&self, project_id: &str, reservation_id: &str, actual_usd: f64) {
        let url = format!(
            "{}/v1/projects/{}/budget/reconcile",
            self.config.base_url, project_id
        );
        let body = ReconcileRequest {
            reservation_id,
            actual_usd,
        };
        if let Err(e) = self
            .http
            .post(&url)
            .header(header::AUTHORIZATION, self.auth_header())
            .json(&body)
            .send()
            .await
        {
            warn!(error = %e, "control-plane reconcile failed; reservation will auto-release on TTL");
        }
    }

    /// Fetch policies with a conditional GET. Pass the last-known ETag to get a
    /// cheap `304` when nothing changed.
    pub async fn fetch_policies(
        &self,
        project_id: &str,
        etag: Option<&str>,
    ) -> Result<PolicyFetch, ControlPlaneError> {
        let url = format!(
            "{}/v1/projects/{}/policies",
            self.config.base_url, project_id
        );

        let mut req = self
            .http
            .get(&url)
            .header(header::AUTHORIZATION, self.auth_header());
        if let Some(tag) = etag {
            req = req.header(header::IF_NONE_MATCH, tag);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ControlPlaneError::Transient(e.to_string()))?;

        match resp.status() {
            StatusCode::NOT_MODIFIED => {
                debug!("policies unchanged (304)");
                Ok(PolicyFetch::NotModified)
            }
            StatusCode::OK => {
                let new_etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| ControlPlaneError::Fatal(e.to_string()))?;
                let bundle = PolicyBundle::from_json_slice(&bytes)
                    .map_err(|e| ControlPlaneError::Fatal(format!("bad policy bundle: {e}")))?;
                Ok(PolicyFetch::Updated {
                    bundle,
                    etag: new_etag,
                })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(ControlPlaneError::Unauthorized)
            }
            s if s.is_server_error() => Err(ControlPlaneError::Transient(format!("status {s}"))),
            s => Err(ControlPlaneError::Fatal(format!("status {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header as match_header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> ControlPlaneClient {
        ControlPlaneClient::new(ControlPlaneConfig {
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            request_timeout: Duration::from_secs(2),
            max_retries: 2,
        })
    }

    #[tokio::test]
    async fn reserve_admits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj_1/budget/reserve"))
            .and(match_header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "admit": true, "reservationId": "r_1", "headroomUsd": 50.0, "currentSpendUsd": 50.0
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client_for(&server);
        let r = c.reserve("proj_1", 10.0, "gpt-4o", "req-1").await.unwrap();
        assert!(r.admit);
        assert_eq!(r.reservation_id.as_deref(), Some("r_1"));
    }

    #[tokio::test]
    async fn reserve_denies() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj_1/budget/reserve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "admit": false, "headroomUsd": 0.0, "currentSpendUsd": 1500.0
            })))
            .mount(&server)
            .await;

        let c = client_for(&server);
        let r = c.reserve("proj_1", 10.0, "gpt-4o", "req-1").await.unwrap();
        assert!(!r.admit);
    }

    #[tokio::test]
    async fn reserve_retries_transient_then_succeeds() {
        let server = MockServer::start().await;
        // First a 503 (transient), then a 200. wiremock serves mounts in order
        // with `up_to_n_times`.
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/budget/reserve"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/budget/reserve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"admit": true})))
            .mount(&server)
            .await;

        let c = client_for(&server);
        let r = c.reserve("p", 1.0, "m", "rid").await.unwrap();
        assert!(r.admit);
    }

    #[tokio::test]
    async fn reserve_unauthorized_is_terminal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/budget/reserve"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.reserve("p", 1.0, "m", "rid").await.unwrap_err();
        assert!(matches!(err, ControlPlaneError::Unauthorized));
    }

    #[tokio::test]
    async fn fetch_policies_returns_bundle_and_etag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/p/policies"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_json(json!({"policies": [
                        {"name": "m", "type": "model_allowlist", "config": {"allowed": ["gpt-4o"]}}
                    ]})),
            )
            .mount(&server)
            .await;

        let c = client_for(&server);
        match c.fetch_policies("p", None).await.unwrap() {
            PolicyFetch::Updated { bundle, etag } => {
                assert_eq!(bundle.policies.len(), 1);
                assert_eq!(etag.as_deref(), Some("\"v1\""));
            }
            _ => panic!("expected Updated"),
        }
    }

    #[tokio::test]
    async fn fetch_policies_304_not_modified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/p/policies"))
            .and(match_header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let c = client_for(&server);
        let r = c.fetch_policies("p", Some("\"v1\"")).await.unwrap();
        assert!(matches!(r, PolicyFetch::NotModified));
    }

    #[tokio::test]
    async fn reconcile_posts_actual() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/p/budget/reconcile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"settled": true})))
            .expect(1)
            .mount(&server)
            .await;
        let c = client_for(&server);
        c.reconcile("p", "r_1", 3.21).await; // best-effort; just must not panic
    }

    #[test]
    fn config_from_env_requires_both() {
        // not set -> None
        std::env::remove_var("NOVEUM_ENDPOINT");
        std::env::remove_var("NOVEUM_API_KEY");
        assert!(ControlPlaneConfig::from_env().is_none());
    }
}
