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
//! mode (see [`CostEnforcementMode`]):
//!
//! 1. `POST .../policies/admit` with this request's *estimated* usage. The
//!    platform reserves atomically and answers allowed / blocked, or 503
//!    (unavailable — never treated as allowed).
//! 2. The reservation is held by an RAII [`AdmissionGuard`] for the whole
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
//! Settlement never blocks the client: [`AdmissionGuard`]'s `Drop` spawns it.
//!
//! Native-only (reqwest). The Worker has no admission client yet; it already
//! refuses to start with `cost_cap`/`rate_limit` policies at all.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

use crate::policy::config::CostEnforcementMode;
use crate::policy::decision::{PolicyAction, PolicyDecision, PolicyMode, Severity};
use crate::policy::remote::{truncate_body, RemoteConfig, RemoteConfigError, PLATFORM_CLIENT};

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

/// The platform's rejection, as returned in a `200 {allowed:false}` body.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockedDecision {
    pub policy_id: String,
    pub policy_name: String,
    /// Gateway-side policy type (`cost_cap` / `rate_limit`), already mapped from
    /// the platform's `COST_CAP` / `RATE_LIMIT`.
    pub policy_type: String,
    pub scope: Option<String>,
    pub dimension: Option<String>,
    pub limit: Option<f64>,
    pub observed: Option<f64>,
    pub projected: Option<f64>,
    pub reason: String,
}

impl BlockedDecision {
    /// Render as the gateway's uniform [`PolicyDecision`] so the platform's
    /// block flows through the same synthetic-response, logging and telemetry
    /// paths as a locally-evaluated one.
    pub fn to_policy_decision(&self) -> PolicyDecision {
        let mut d = PolicyDecision::allow(
            &self.policy_id,
            &self.policy_name,
            &self.policy_type,
            PolicyMode::Enforce,
        );
        d.flagged = true;
        d.score = 1.0;
        d.severity = Severity::Critical;
        d.action = PolicyAction::Block;
        d.reason = self.reason.clone();
        d
    }
}

/// An accepted reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: String,
    pub expires_at: Option<String>,
    pub policy_version: Option<String>,
    /// `true` when this call replayed an existing reservation for the same
    /// `requestId` (a retry) rather than creating a new one.
    pub replayed: bool,
}

/// The outcome of one admission call.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    Allowed(Reservation),
    Blocked(Box<BlockedDecision>),
    /// Admission could not be evaluated (503, transport failure, budget
    /// exceeded, malformed answer). The caller applies `failClosed`: this is
    /// never an allow by itself.
    Unavailable(String),
}

/// This request's estimated usage, as sent to `/admit`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmitRequest {
    /// Idempotency key. One per logical request; reused on retry.
    pub request_id: String,
    pub provider: Option<String>,
    pub model: String,
    pub estimated_input_tokens: u64,
    pub maximum_output_tokens: u64,
    pub estimated_cost_usd: f64,
}

impl AdmitRequest {
    fn to_json(&self) -> Value {
        let mut v = serde_json::json!({
            "requestId": self.request_id,
            "model": self.model,
            "estimatedInputTokens": self.estimated_input_tokens,
            "maximumOutputTokens": self.maximum_output_tokens,
            // NaN/negative would serialize as `null` / be rejected by the
            // schema (`number >= 0`); clamp to a value the platform accepts.
            "estimatedCostUsd": sanitize_cost(self.estimated_cost_usd),
        });
        if let Some(p) = &self.provider {
            v["provider"] = Value::from(p.as_str());
        }
        v
    }
}

/// Coerce an estimated cost into the platform's accepted range (`number >= 0`),
/// mapping NaN to 0 rather than emitting a `null` the schema rejects.
fn sanitize_cost(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v.min(100_000.0)
    } else {
        0.0
    }
}

/// Authoritative usage for a completed request.
#[derive(Debug, Clone, PartialEq)]
pub struct SettlementUsage {
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_count: u64,
    /// Idempotency key for the underlying usage record (optional server-side).
    pub event_id: Option<String>,
}

/// How a held reservation is closed out.
#[derive(Debug, Clone, PartialEq)]
pub enum Settlement {
    /// Real usage recovered: apply it in place of the estimate.
    Complete(Box<SettlementUsage>),
    /// The call may have reached the provider but no authoritative usage was
    /// recovered. The conservative estimate STAYS applied.
    Abandon(String),
    /// The call provably never reached the provider. Releases the hold.
    Cancel(String),
}

impl Settlement {
    /// URL segment under `/reservations/{id}/`.
    fn endpoint(&self) -> &'static str {
        match self {
            Settlement::Complete(_) => "complete",
            Settlement::Abandon(_) => "abandon",
            Settlement::Cancel(_) => "cancel",
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Settlement::Complete(u) => {
                let mut v = serde_json::json!({
                    "inputTokens": u.input_tokens,
                    "outputTokens": u.output_tokens,
                    "costUsd": sanitize_cost(u.cost_usd),
                    "requestCount": u.request_count,
                    "timestamp": now_iso(),
                });
                if let Some(m) = &u.model {
                    v["model"] = Value::from(m.as_str());
                }
                if let Some(e) = &u.event_id {
                    v["eventId"] = Value::from(e.as_str());
                }
                v
            }
            Settlement::Abandon(reason) | Settlement::Cancel(reason) => {
                serde_json::json!({ "reason": reason })
            }
        }
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Classify an `/admit` HTTP response. Pure, so the whole contract (including
/// the counter-intuitive "blocked is HTTP 200") is unit-testable without a
/// server.
///
/// Anything that is not an unambiguous allow or an unambiguous block is
/// [`Admission::Unavailable`] — the caller then applies `failClosed`. There is
/// deliberately no path from a malformed or unexpected response to "allowed".
pub fn classify_admit(status: u16, body: &[u8]) -> Admission {
    let text = String::from_utf8_lossy(body);
    let json: Option<Value> = serde_json::from_slice(body).ok();

    if status == 503 {
        return Admission::Unavailable(format!(
            "admission unavailable (503): {}",
            json.as_ref()
                .and_then(message_of)
                .unwrap_or_else(|| truncate_body(&text))
        ));
    }
    if !(200..300).contains(&status) {
        return Admission::Unavailable(format!(
            "admit returned {status}: {}",
            truncate_body(&text)
        ));
    }
    let Some(json) = json else {
        return Admission::Unavailable(format!(
            "admit returned invalid JSON: {}",
            truncate_body(&text)
        ));
    };
    match json.get("allowed").and_then(|v| v.as_bool()) {
        Some(true) => {
            // An "allowed" with no reservation id cannot be settled, so the
            // platform's hold would linger for its whole TTL. That is a
            // platform-side bug; treat it as unavailable rather than silently
            // admitting a request we can never close out.
            let Some(id) = json
                .get("reservationId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            else {
                return Admission::Unavailable(
                    "admit returned allowed with no reservationId".to_string(),
                );
            };
            Admission::Allowed(Reservation {
                id: id.to_string(),
                expires_at: str_field(&json, "expiresAt"),
                policy_version: str_field(&json, "policyVersion"),
                replayed: json
                    .get("replayed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        }
        Some(false) => Admission::Blocked(Box::new(parse_decision(&json))),
        None => Admission::Unavailable(format!(
            "admit response has no `allowed` field: {}",
            truncate_body(&text)
        )),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Best-effort error message out of a platform error envelope.
fn message_of(v: &Value) -> Option<String> {
    for key in ["message", "error", "code"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
    }
    v.pointer("/error/message")
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// Parse the `decision` object of a `200 {allowed:false}` body. A block with a
/// missing or malformed decision is still a block (the platform said no) — it
/// just gets generic labels.
fn parse_decision(body: &Value) -> BlockedDecision {
    let d = body.get("decision").unwrap_or(&Value::Null);
    let policy_type = match d.get("policyType").and_then(|v| v.as_str()) {
        Some(t) if t.eq_ignore_ascii_case("RATE_LIMIT") => "rate_limit",
        // COST_CAP, or anything unrecognized: a cost cap is the conservative
        // label for an admission block and keeps `blockedBy` mapping valid.
        _ => "cost_cap",
    };
    let num = |k: &str| d.get(k).and_then(|v| v.as_f64());
    BlockedDecision {
        policy_id: str_field(d, "policyId").unwrap_or_else(|| "platform_admission".to_string()),
        policy_name: str_field(d, "policyName").unwrap_or_else(|| "platform admission".to_string()),
        policy_type: policy_type.to_string(),
        scope: str_field(d, "scope"),
        dimension: str_field(d, "dimension"),
        limit: num("limit"),
        observed: num("observed"),
        projected: num("projected"),
        reason: str_field(d, "reason")
            .unwrap_or_else(|| "blocked by platform admission control".to_string()),
    }
}

/// Decide whether a request uses platform-atomic admission.
///
/// * an explicit `NOVEUM_GUARD_COST_ENFORCEMENT` overrides every policy (a
///   deployment-wide kill switch, and a way to force strict without editing
///   policies);
/// * otherwise the active policy set decides — strict when any active
///   `cost_cap` declares `enforcementMode: strict`.
pub fn resolve_strict(override_mode: Option<CostEnforcementMode>, policy_strict: bool) -> bool {
    match override_mode {
        Some(CostEnforcementMode::Strict) => true,
        Some(CostEnforcementMode::Advisory) => false,
        None => policy_strict,
    }
}

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
        let body = settlement.to_json();
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

/// Percent-encode the few characters that could break out of a path segment. A
/// reservation id is a server-issued UUID, so this is belt-and-braces against a
/// future id format rather than a live concern.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
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

    fn allowed_body() -> Vec<u8> {
        br#"{"allowed":true,"reservationId":"res-1","expiresAt":"2026-08-12T00:00:00Z",
             "policyVersion":"\"etag\"","replayed":false,"shadowed":[]}"#
            .to_vec()
    }

    #[test]
    fn allowed_response_yields_a_reservation() {
        match classify_admit(200, &allowed_body()) {
            Admission::Allowed(r) => {
                assert_eq!(r.id, "res-1");
                assert_eq!(r.expires_at.as_deref(), Some("2026-08-12T00:00:00Z"));
                assert_eq!(r.policy_version.as_deref(), Some("\"etag\""));
                assert!(!r.replayed);
            }
            other => panic!("expected Allowed, got {other:?}"),
        }
    }

    #[test]
    fn replayed_flag_is_carried_through() {
        let body = br#"{"allowed":true,"reservationId":"res-1","replayed":true}"#;
        match classify_admit(200, body) {
            Admission::Allowed(r) => assert!(r.replayed, "a retried requestId replays"),
            other => panic!("expected Allowed, got {other:?}"),
        }
    }

    /// The contract's sharpest edge: a block is HTTP **200**, not a 4xx.
    #[test]
    fn blocked_is_http_200_with_a_decision() {
        let body = br#"{"allowed":false,"decision":{"policyId":"pol_1","policyName":"Org cap",
            "policyType":"COST_CAP","scope":"org","dimension":"7d_rolling","limit":1200,
            "observed":1199.5,"projected":1200.5,"reason":"org 7d cap reached"}}"#;
        match classify_admit(200, body) {
            Admission::Blocked(d) => {
                assert_eq!(d.policy_id, "pol_1");
                assert_eq!(d.policy_name, "Org cap");
                assert_eq!(d.policy_type, "cost_cap", "mapped to the gateway's type");
                assert_eq!(d.scope.as_deref(), Some("org"));
                assert_eq!(d.dimension.as_deref(), Some("7d_rolling"));
                assert_eq!(d.limit, Some(1200.0));
                assert_eq!(d.observed, Some(1199.5));
                assert_eq!(d.projected, Some(1200.5));
                assert_eq!(d.reason, "org 7d cap reached");
                // ...and it renders as a normal enforced block decision.
                let pd = d.to_policy_decision();
                assert!(pd.is_blocking());
                assert_eq!(pd.policy_type, "cost_cap");
                assert_eq!(
                    crate::policy::usage::blocked_by_for(&pd.policy_type),
                    Some("COST_CAP"),
                    "the block must be reportable as a usage block"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_block_maps_to_the_rate_limit_type() {
        let body = br#"{"allowed":false,"decision":{"policyId":"p","policyName":"n",
            "policyType":"RATE_LIMIT","reason":"too many"}}"#;
        match classify_admit(200, body) {
            Admission::Blocked(d) => {
                assert_eq!(d.policy_type, "rate_limit");
                assert_eq!(
                    crate::policy::usage::blocked_by_for(&d.policy_type),
                    Some("RATE_LIMIT")
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn a_block_without_a_decision_is_still_a_block() {
        match classify_admit(200, br#"{"allowed":false}"#) {
            Admission::Blocked(d) => {
                assert!(!d.reason.is_empty());
                assert_eq!(d.policy_type, "cost_cap");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn service_unavailable_is_never_an_allow() {
        for body in [
            &br#"{"message":"GUARDRAIL_ADMISSION_UNAVAILABLE"}"#[..],
            &br#"{"message":"GUARDRAIL_POLICY_UNENFORCEABLE"}"#[..],
            &b"<html>gateway timeout</html>"[..],
        ] {
            match classify_admit(503, body) {
                Admission::Unavailable(r) => assert!(r.contains("503"), "reason kept: {r}"),
                other => panic!("503 must be Unavailable, got {other:?}"),
            }
        }
        // The specific platform code is preserved for the operator.
        let Admission::Unavailable(r) =
            classify_admit(503, br#"{"message":"GUARDRAIL_ADMISSION_UNAVAILABLE"}"#)
        else {
            panic!("expected Unavailable");
        };
        assert!(r.contains("GUARDRAIL_ADMISSION_UNAVAILABLE"), "{r}");
    }

    #[test]
    fn other_failures_and_malformed_bodies_are_unavailable_not_allowed() {
        for (status, body) in [
            (500u16, &b"boom"[..]),
            (401, &br#"{"error":"bad key"}"#[..]),
            (429, &b""[..]),
            (200, &b"not json at all"[..]),
            (200, &br#"{"reservationId":"res-1"}"#[..]), // no `allowed`
            (200, &br#"{"allowed":true}"#[..]),          // allowed, unsettleable
            (200, &br#"{"allowed":true,"reservationId":"  "}"#[..]),
        ] {
            assert!(
                matches!(classify_admit(status, body), Admission::Unavailable(_)),
                "status={status} body={:?} must be Unavailable",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn admit_request_serializes_to_the_platform_shape() {
        let v = AdmitRequest {
            request_id: "req-1".into(),
            provider: Some("openai".into()),
            model: "gpt-5.6-luna".into(),
            estimated_input_tokens: 1200,
            maximum_output_tokens: 4096,
            estimated_cost_usd: 0.0051552,
        }
        .to_json();
        assert_eq!(v["requestId"], "req-1");
        assert_eq!(v["provider"], "openai");
        assert_eq!(v["model"], "gpt-5.6-luna");
        assert_eq!(v["estimatedInputTokens"], 1200);
        assert_eq!(v["maximumOutputTokens"], 4096);
        assert_eq!(v["estimatedCostUsd"], 0.0051552);

        // Provider is optional and simply omitted.
        let v = AdmitRequest {
            request_id: "req-2".into(),
            provider: None,
            model: "m".into(),
            estimated_input_tokens: 0,
            maximum_output_tokens: 0,
            estimated_cost_usd: f64::NAN,
        }
        .to_json();
        assert!(v.get("provider").is_none());
        assert_eq!(v["estimatedCostUsd"], 0.0, "NaN must not become null");
    }

    #[test]
    fn settlement_bodies_and_endpoints() {
        let c = Settlement::Complete(Box::new(SettlementUsage {
            model: Some("gpt-4o".into()),
            input_tokens: 123,
            output_tokens: 45,
            cost_usd: 0.0031,
            request_count: 1,
            event_id: Some("evt-1".into()),
        }));
        assert_eq!(c.endpoint(), "complete");
        let v = c.to_json();
        assert_eq!(v["inputTokens"], 123);
        assert_eq!(v["outputTokens"], 45);
        assert_eq!(v["costUsd"], 0.0031);
        assert_eq!(v["requestCount"], 1);
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["eventId"], "evt-1");
        assert!(v["timestamp"].as_str().is_some());

        let a = Settlement::Abandon("stream ended without usage".into());
        assert_eq!(a.endpoint(), "abandon");
        assert_eq!(a.to_json()["reason"], "stream ended without usage");

        let x = Settlement::Cancel("blocked before dispatch".into());
        assert_eq!(x.endpoint(), "cancel");
        assert_eq!(x.to_json()["reason"], "blocked before dispatch");
    }

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
    #[test]
    fn drop_defaults_to_abandon() {
        let settlement = Settlement::Abandon("x".into());
        assert_eq!(settlement.endpoint(), "abandon");
        // The default is constructed in `Drop`; assert the same choice here so
        // a future edit that flips it to `cancel` fails a test.
        let default_on_drop =
            Settlement::Abandon("request ended without authoritative usage".to_string());
        assert_eq!(default_on_drop.endpoint(), "abandon");
    }
}
