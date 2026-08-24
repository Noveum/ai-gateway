//! Cross-replica atomic admission against the platform's reservation API.
//!
//! The in-process [`PendingSpend`](crate::policy::remote::PendingSpend) ledger
//! closes the "counters lag" window *within one gateway process*. It cannot
//! close it across replicas: with 50 pods each holding its own ledger, a $100
//! org cap is enforced 50 times over and leaks toward $5,000. The platform now
//! exposes an atomic admission API — one authoritative counter, one reservation
//! per request — and this module is the gateway-side client for it.
//!
//! Flow, per guarded request, when a `cost_cap` is in **strict** enforcement
//! mode (see [`CostEnforcementMode`](crate::policy::config::CostEnforcementMode)):
//!
//! 1. `POST .../policies/admit` with this request's *estimated* usage. The
//!    platform reserves atomically and answers allowed / blocked, or 503
//!    (unavailable — never treated as allowed).
//! 2. The reservation is held by an RAII
//!    [`AdmissionGuard`](crate::policy::admission::AdmissionGuard) for the whole
//!    request, exactly like [`ReservationGuard`](crate::policy::remote::ReservationGuard).
//! 3. On the way out the guard **settles**:
//!    * `complete` with the real token counts when authoritative usage was
//!      recovered from the response;
//!    * `abandon` when the call may have reached the provider but no usage came
//!      back (a stream that ended without a usage chunk, a client that vanished
//!      mid-body) — the conservative estimate stays applied;
//!    * `cancel` only when the request provably never reached the provider (a
//!      later gateway-side policy blocked it), which releases the hold.
//!
//! Settlement never blocks the client:
//! [`AdmissionGuard`](crate::policy::admission::AdmissionGuard)'s `Drop` spawns
//! it.
//!
//! This module is the native `reqwest` transport. The Worker performs the same
//! atomic admission and settlement through [`crate::policy::worker_remote`],
//! using the shared wire contract in [`crate::policy::admission_wire`].

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use crate::policy::admission_wire::truncate_body;
use crate::policy::config::CostEnforcementMode;
use crate::policy::remote::{RemoteConfig, RemoteConfigError, PLATFORM_CLIENT};

/// Selects strict (platform-atomic) vs advisory (in-process ledger) cost
/// enforcement for the whole gateway, overriding each policy's own
/// `enforcementMode`. Unset → the policy decides.
pub const COST_ENFORCEMENT_VAR: &str = "NOVEUM_GUARD_COST_ENFORCEMENT";

/// Hard bound on an in-request admission call, mirroring
/// `remote::STATE_REFRESH_BUDGET`. Admission is on the critical path: a slow
/// control plane must not stall the request for the client's whole timeout.
/// Exceeding it is [`Admission::Unavailable`] — **never** an implicit allow.
const ADMIT_BUDGET: Duration = Duration::from_secs(2);
/// Attempts for one admission call. Retrying the same `requestId` returns the
/// same reservation (`replayed: true`) instead of reserving twice, so a
/// transport failure is safe to retry — and must be, otherwise a dropped
/// connection turns into a spurious block on a fail-closed policy.
const ADMIT_ATTEMPTS: u32 = 2;
/// Pause between admission attempts. Deliberately tiny: the whole call still
/// has to fit inside [`ADMIT_BUDGET`].
const ADMIT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Attempts for a settlement call. Settlement runs off the request path, so it
/// can afford real backoff; every endpoint is idempotent per reservation.
const SETTLE_ATTEMPTS: u32 = 3;
/// Base delay for settlement backoff (doubles per attempt).
const SETTLE_BASE_DELAY: Duration = Duration::from_millis(250);

// The wire contract -- the request/settlement bodies, `/admit` classification
// and URL-segment encoding -- lives in `admission_wire`, shared byte-for-byte
// with the Worker client. Only the `reqwest` transport and the RAII settlement
// guard are native, and they are what remains in this file.
pub use crate::policy::admission_wire::{
    classify_admit, urlencode, Admission, AdmitRequest, BlockedDecision, Reservation, Settlement,
    SettlementUsage,
};

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The routing predicate, defined in [`crate::policy::config`] so that the
/// fail-closed branch in [`crate::policy::engine`] — which is also compiled for
/// the wasm32 Worker, where this module is not — can select exactly the caps
/// this function routed through admission.
pub use crate::policy::config::resolve_strict;

/// Parse the `NOVEUM_GUARD_COST_ENFORCEMENT` value.
///
/// Follows the [`RemoteConfig`] convention: absent means "not configured", but
/// anything *present* and unusable is a loud error rather than a silent
/// fallback — an operator who typo'd `strcit` must not get advisory
/// (per-replica, multiplied) enforcement while believing the cap is atomic.
pub fn enforcement_from_value(
    raw: Option<&str>,
    platform_configured: bool,
) -> Result<Option<CostEnforcementMode>, RemoteConfigError> {
    let err = |message: String| Err(RemoteConfigError { message });
    let Some(raw) = raw else {
        return Ok(None);
    };
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
        return err(format!(
            "{COST_ENFORCEMENT_VAR} is set but empty; set it to `strict` or `advisory`, or unset it \
             to let each policy's own `enforcementMode` decide."
        ));
    }
    match v.as_str() {
        "advisory" => Ok(Some(CostEnforcementMode::Advisory)),
        "strict" if !platform_configured => err(format!(
            "{COST_ENFORCEMENT_VAR}=strict requires the platform bridge \
             ({} + {}), which is not configured; strict enforcement has no admission API to call.",
            crate::policy::remote::API_KEY_VAR,
            crate::policy::remote::PROJECT_ID_VAR
        )),
        "strict" => Ok(Some(CostEnforcementMode::Strict)),
        _ => err(format!(
            "{COST_ENFORCEMENT_VAR}={raw:?} is not a known enforcement mode; use `strict` or `advisory`."
        )),
    }
}

/// HTTP client for the platform's admission + reservation endpoints.
pub struct AdmissionClient {
    cfg: RemoteConfig,
    /// Deployment-wide override of each policy's `enforcementMode`.
    mode_override: Option<CostEnforcementMode>,
    budget: Duration,
}

impl AdmissionClient {
    pub fn new(cfg: RemoteConfig, mode_override: Option<CostEnforcementMode>) -> Self {
        Self {
            cfg,
            mode_override,
            budget: ADMIT_BUDGET,
        }
    }

    /// Like [`AdmissionClient::new`] with an explicit request-path budget (tests).
    pub fn with_budget(
        cfg: RemoteConfig,
        mode_override: Option<CostEnforcementMode>,
        budget: Duration,
    ) -> Self {
        Self {
            cfg,
            mode_override,
            budget,
        }
    }

    /// Build from the environment. `remote` is the already-validated platform
    /// bridge configuration (`None` = no bridge). Returns `Ok(None)` when
    /// admission is not available at all; `Err` when the configuration is
    /// present but unusable (startup must abort rather than quietly degrade to
    /// per-replica enforcement).
    pub fn from_env(remote: Option<&RemoteConfig>) -> Result<Option<Self>, RemoteConfigError> {
        let raw = std::env::var(COST_ENFORCEMENT_VAR).ok();
        let mode = enforcement_from_value(raw.as_deref(), remote.is_some())?;
        Ok(remote.map(|cfg| Self::new(cfg.clone(), mode)))
    }

    /// Does this request use platform-atomic admission? `policy_strict` is
    /// whether the active policy set contains a strict `cost_cap`.
    pub fn strict_for(&self, policy_strict: bool) -> bool {
        resolve_strict(self.mode_override, policy_strict)
    }

    /// The deployment-wide `NOVEUM_GUARD_COST_ENFORCEMENT` override this client
    /// routes with, resolved once at startup.
    ///
    /// Exposed so the fail-closed branch taken when `/admit` is unavailable can
    /// select the same caps [`AdmissionClient::strict_for`] routed *into*
    /// admission, without re-reading the environment per request.
    pub fn mode_override(&self) -> Option<CostEnforcementMode> {
        self.mode_override
    }

    fn admit_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/admit",
            self.cfg.base_url, self.cfg.project_id
        )
    }

    fn reservation_url(&self, id: &str, endpoint: &str) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/reservations/{}/{}",
            self.cfg.base_url,
            self.cfg.project_id,
            urlencode(id),
            endpoint
        )
    }

    /// Reserve this request's estimated usage atomically, platform-side.
    ///
    /// Bounded by the request-path budget; exceeding it is
    /// [`Admission::Unavailable`], never an allow.
    pub async fn admit(&self, req: &AdmitRequest) -> Admission {
        match tokio::time::timeout(self.budget, self.admit_inner(req)).await {
            Ok(outcome) => outcome,
            Err(_) => Admission::Unavailable(format!(
                "admission exceeded the {}ms request budget",
                self.budget.as_millis()
            )),
        }
    }

    async fn admit_inner(&self, req: &AdmitRequest) -> Admission {
        let url = self.admit_url();
        let body = req.to_json();
        let mut last_error: Option<String> = None;
        for attempt in 1..=ADMIT_ATTEMPTS {
            let resp = PLATFORM_CLIENT
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .json(&body)
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    // A gateway-level blip (502/504/500, or a 429) is worth one
                    // more try: the same `requestId` replays, so this cannot
                    // double-reserve. A **503** is not retried — that is the
                    // platform explicitly saying admission is unevaluable, and
                    // hammering it only adds latency to an outage.
                    let retryable =
                        status != 503 && (status == 429 || (500..600).contains(&status));
                    if retryable && attempt < ADMIT_ATTEMPTS {
                        last_error = Some(format!("admit returned {status}"));
                        tokio::time::sleep(ADMIT_RETRY_DELAY).await;
                        continue;
                    }
                    let bytes = r.bytes().await.unwrap_or_default();
                    let outcome = classify_admit(status, &bytes);
                    if let Admission::Allowed(res) = &outcome {
                        debug!(
                            reservation = %res.id, replayed = res.replayed, model = %req.model,
                            "Nova Guard: platform admission granted"
                        );
                    }
                    return outcome;
                }
                Err(e) => {
                    // Safe to retry: the same `requestId` replays the same
                    // reservation instead of reserving twice.
                    last_error = Some(e.to_string());
                    if attempt < ADMIT_ATTEMPTS {
                        tokio::time::sleep(ADMIT_RETRY_DELAY).await;
                    }
                }
            }
        }
        Admission::Unavailable(format!(
            "admit failed after {ADMIT_ATTEMPTS} attempts: {}",
            last_error.unwrap_or_else(|| "unknown transport error".to_string())
        ))
    }

    /// Close out a reservation. Idempotent per reservation id; retries
    /// transient failures. Runs off the request path (see [`AdmissionGuard`]).
    pub async fn settle(&self, reservation_id: &str, settlement: &Settlement) -> bool {
        let url = self.reservation_url(reservation_id, settlement.endpoint());
        let body = settlement.to_json(Some(&now_iso()));
        for attempt in 1..=SETTLE_ATTEMPTS {
            let resp = PLATFORM_CLIENT
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .json(&body)
                .send()
                .await;
            let transient = match resp {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        debug!(
                            reservation = %reservation_id, endpoint = settlement.endpoint(),
                            "Nova Guard: reservation settled"
                        );
                        return true;
                    }
                    if status.as_u16() == 429 || status.is_server_error() {
                        true
                    } else {
                        let body = r.text().await.unwrap_or_default();
                        warn!(
                            reservation = %reservation_id, endpoint = settlement.endpoint(),
                            %status, body = %truncate_body(&body),
                            "Nova Guard: reservation settlement rejected; giving up"
                        );
                        return false;
                    }
                }
                Err(e) => {
                    warn!(
                        reservation = %reservation_id, endpoint = settlement.endpoint(),
                        error = %e, attempt, "Nova Guard: reservation settlement failed"
                    );
                    true
                }
            };
            if transient && attempt < SETTLE_ATTEMPTS {
                let mult = 1u32 << (attempt - 1).min(16);
                tokio::time::sleep(SETTLE_BASE_DELAY * mult).await;
            }
        }
        warn!(
            reservation = %reservation_id, endpoint = settlement.endpoint(),
            "Nova Guard: reservation settlement gave up after retries"
        );
        false
    }
}

/// RAII settlement guard for a held platform reservation.
///
/// Mirrors [`ReservationGuard`](crate::policy::remote::ReservationGuard): it is
/// created the moment admission is granted and then moved along the request —
/// held by the middleware future while it awaits upstream headers, then
/// transferred into the outgoing response body. **Every** exit path settles:
///
/// * response fully read with authoritative usage → `complete`;
/// * body finished or client disconnected with no usage → `abandon`
///   (conservative: the estimate stays applied);
/// * a later gateway policy blocked the call before dispatch → `cancel`;
/// * the middleware future itself dropped (client gone before upstream headers)
///   → `abandon`, because the request may well have reached the provider.
///
/// `Drop` cannot await, so it spawns the settlement — which is also what keeps
/// settlement off the client's critical path.
pub struct AdmissionGuard {
    client: Arc<AdmissionClient>,
    reservation_id: String,
    /// `None` until an explicit outcome is chosen; `Drop` defaults to abandon.
    outcome: Option<Settlement>,
}

impl AdmissionGuard {
    pub fn new(client: Arc<AdmissionClient>, reservation_id: String) -> Self {
        Self {
            client,
            reservation_id,
            outcome: None,
        }
    }

    /// The held reservation id (diagnostics/tests).
    pub fn reservation_id(&self) -> &str {
        &self.reservation_id
    }

    /// Settle with authoritative usage. Consumes the guard: `Drop` sends it.
    pub fn complete(mut self, usage: SettlementUsage) {
        self.outcome = Some(Settlement::Complete(Box::new(usage)));
    }

    /// Release the hold — only valid when the request provably never reached
    /// the provider.
    pub fn cancel(mut self, reason: impl Into<String>) {
        self.outcome = Some(Settlement::Cancel(reason.into()));
    }

    /// Keep the estimate applied: the call may have reached the provider but no
    /// authoritative usage was recovered. This is also the `Drop` default.
    pub fn abandon(mut self, reason: impl Into<String>) {
        self.outcome = Some(Settlement::Abandon(reason.into()));
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let settlement = self.outcome.take().unwrap_or_else(|| {
            Settlement::Abandon("request ended without authoritative usage".to_string())
        });
        let client = self.client.clone();
        let id = std::mem::take(&mut self.reservation_id);
        if id.is_empty() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    client.settle(&id, &settlement).await;
                });
            }
            // Outside a runtime (process teardown): nothing can be sent. The
            // reservation expires server-side at `expiresAt` — conservatively,
            // i.e. still counted until then.
            Err(_) => warn!(
                reservation = %id, endpoint = settlement.endpoint(),
                "Nova Guard: no tokio runtime at settlement time; reservation left to expire"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> RemoteConfig {
        RemoteConfig {
            base_url: "https://api.example.com".into(),
            api_key: "k".into(),
            project_id: "proj_1".into(),
        }
    }

    #[test]
    fn urls_match_the_documented_contract() {
        let c = AdmissionClient::new(test_cfg(), None);
        assert_eq!(
            c.admit_url(),
            "https://api.example.com/api/v1/projects/proj_1/policies/admit"
        );
        assert_eq!(
            c.reservation_url("res-9", "complete"),
            "https://api.example.com/api/v1/projects/proj_1/policies/reservations/res-9/complete"
        );
        // Path-hostile ids can't escape their segment.
        assert!(c
            .reservation_url("../../evil", "cancel")
            .ends_with("/reservations/..%2F..%2Fevil/cancel"));
    }

    #[test]
    fn strict_selection_is_a_pure_decision() {
        // No override: the policy set decides.
        assert!(resolve_strict(None, true));
        assert!(!resolve_strict(None, false));
        // An explicit override wins either way (deployment kill switch / force).
        assert!(resolve_strict(Some(CostEnforcementMode::Strict), false));
        assert!(!resolve_strict(Some(CostEnforcementMode::Advisory), true));
    }

    #[test]
    fn enforcement_env_parsing_fails_loudly_when_set_but_unusable() {
        // Unset → not configured.
        assert_eq!(enforcement_from_value(None, true), Ok(None));
        // Recognized values.
        assert_eq!(
            enforcement_from_value(Some(" Strict "), true),
            Ok(Some(CostEnforcementMode::Strict))
        );
        assert_eq!(
            enforcement_from_value(Some("ADVISORY"), false),
            Ok(Some(CostEnforcementMode::Advisory))
        );
        // Present but unusable: empty, typo'd, or strict with no platform bridge.
        for (raw, platform, needle) in [
            (Some(""), true, "empty"),
            (Some("   "), true, "empty"),
            (Some("strcit"), true, "not a known enforcement mode"),
            (Some("1"), true, "not a known enforcement mode"),
            (Some("strict"), false, "requires the platform bridge"),
        ] {
            let e = enforcement_from_value(raw, platform)
                .expect_err("a set-but-unusable value must be an error");
            assert!(
                e.message.contains(needle),
                "unhelpful message for {raw:?}: {}",
                e.message
            );
        }
    }

    #[test]
    fn strict_for_combines_the_override_with_the_policy_set() {
        let forced = AdmissionClient::new(test_cfg(), Some(CostEnforcementMode::Strict));
        assert!(forced.strict_for(false));
        let disabled = AdmissionClient::new(test_cfg(), Some(CostEnforcementMode::Advisory));
        assert!(!disabled.strict_for(true));
        let by_policy = AdmissionClient::new(test_cfg(), None);
        assert!(by_policy.strict_for(true));
        assert!(!by_policy.strict_for(false));
    }

    /// A guard that is simply dropped must abandon (estimate retained), never
    /// silently vanish and never cancel (which would release a hold for a call
    /// that may have reached the provider).
    #[tokio::test]
    async fn drop_defaults_to_abandon() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let expected_path = "/api/v1/projects/proj_1/policies/reservations/res-drop/abandon";
        Mock::given(method("POST"))
            .and(path(expected_path))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = Arc::new(AdmissionClient::new(
            RemoteConfig {
                base_url: server.uri(),
                ..test_cfg()
            },
            None,
        ));
        drop(AdmissionGuard::new(client, "res-drop".into()));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let requests = server.received_requests().await.unwrap_or_default();
                if requests
                    .iter()
                    .any(|request| request.url.path() == expected_path)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping an undecided guard must settle through /abandon");

        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == expected_path)
                .count(),
            1,
            "Drop must abandon the held reservation exactly once"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().ends_with("/cancel")),
            "Drop must never release a hold for a call that may have reached the provider"
        );
    }
}
