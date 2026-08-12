//! Cloudflare Worker control-plane client for platform-managed Nova Guard.
//!
//! The native gateway reaches the Noveum platform through [`crate::policy::remote`]
//! (policies + live state) and [`crate::policy::admission`] (atomic reservations).
//! Both are built on `reqwest`, which does not exist on `wasm32-unknown-unknown`,
//! so the Worker could not talk to the platform at all — it answered every
//! platform-managed configuration with a `503 gateway_configuration_error`.
//!
//! This module is the `wasm32` replacement for that pair. It speaks the **same
//! wire contract** over `worker::Fetch`:
//!
//! * `GET  {base}/api/v1/projects/{id}/policies/effective` — conditional
//!   (`If-None-Match`) fetch of the merged, enabled-only, priority-ordered set.
//! * `GET  {base}/api/v1/projects/{id}/policies/state` — live cost/rate counters.
//! * `POST {base}/api/v1/projects/{id}/policies/admit` — atomic admission.
//! * `POST {base}/api/v1/projects/{id}/policies/reservations/{rid}/{complete
//!   |abandon|cancel}` — settlement.
//!
//! # Layout: pure core, thin I/O
//!
//! Everything that decides something — URL shaping, the credential matrix, the
//! `/admit` response classification (a **block is HTTP 200**, a 503 is *never*
//! an allow), the settlement choice, the request-body cap, the
//! `stream_options.include_usage` override — is pure, target-independent and
//! covered by the tests at the bottom of this file, which run on the ordinary
//! `cargo test` path. Only the four `worker::Fetch` calls are `wasm32`-gated,
//! and they contain no policy logic.
//!
//! That is why the module itself is *not* `cfg(target_arch = "wasm32")`: gating
//! the whole file would make its decision logic unreachable from a native test
//! run, which is exactly the part that must not be trusted to a build that
//! cannot be executed here.
//!
//! # Deliberate duplication
//!
//! [`classify_admit`], the settlement bodies and the credential matrix mirror
//! [`crate::policy::admission`] / [`crate::policy::remote`] almost line for
//! line. They are duplicated rather than shared because those modules are
//! `reqwest`-bound at the *module* level; hoisting their pure halves into a
//! shared wasm-safe module is the right follow-up, and the tests here assert the
//! same contract points so the two copies cannot silently diverge.

use serde_json::{json, Value};

use crate::policy::decision::{PolicyAction, PolicyDecision, PolicyMode, Severity};
use crate::policy::metering::ActualUsage;

pub use crate::policy::platform::{API_KEY_VAR, PROJECT_ID_VAR, TENANCY_VAR};

/// Platform base URL override.
pub const API_URL_VAR: &str = "NOVEUM_API_URL";
/// Emergency override: serve traffic when the *first* policy fetch fails.
/// Mirrors `remote::ALLOW_UNGUARDED_START_VAR` on the native side.
pub const ALLOW_UNGUARDED_START_VAR: &str = "NOVEUM_GUARD_ALLOW_UNGUARDED_START";

/// Default platform base URL when [`API_URL_VAR`] is unset.
pub const DEFAULT_API_URL: &str = "https://api.noveum.ai";

/// Largest `max_tokens` we will forward into the admission arithmetic. Mirrors
/// `middleware::MAX_OUTPUT_TOKEN_LIMIT`; anything above it is untrusted client
/// JSON, not a real completion budget.
pub const MAX_OUTPUT_TOKEN_LIMIT: u64 = 10_000_000;

/// The `max_tokens`-style keys a chat-completions body may carry, in the order
/// the gateway honors them.
const OUTPUT_LIMIT_KEYS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Connection details for the platform NovaGuard API, as configured on the
/// Worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRemoteConfig {
    pub base_url: String,
    pub api_key: String,
    pub project_id: String,
}

impl WorkerRemoteConfig {
    /// Decide what a given pair of configuration values means.
    ///
    /// Byte-for-byte the same matrix as `RemoteConfig::from_values` on native,
    /// because the two must not disagree about what a half-applied secret set
    /// means:
    ///
    /// * neither variable present → `Ok(None)` (bridge intentionally off);
    /// * both present and non-blank → `Ok(Some(config))`;
    /// * exactly one present, or either present but blank → `Err(message)`.
    ///
    /// The Worker reaches this with `env.secret(k)` falling back to
    /// `env.var(k)`, so `NOVEUM_API_KEY=""` — an unresolved template or a
    /// stripped CI variable — is an error, never a silent unguarded proxy.
    pub fn from_values(
        api_key: Option<&str>,
        project_id: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let blank = |v: Option<&str>| v.is_some_and(|s| s.trim().is_empty());

        if blank(api_key) {
            return Err(format!(
                "{API_KEY_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide a Noveum service key with `guardrails:read` + `guardrails:ingest` \
                 (`wrangler secret put {API_KEY_VAR}`), or unset both {API_KEY_VAR} and \
                 {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }
        if blank(project_id) {
            return Err(format!(
                "{PROJECT_ID_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide the project id to enforce for, or unset both {API_KEY_VAR} and \
                 {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }

        let (api_key, project_id) = match (api_key, project_id) {
            (None, None) => return Ok(None),
            (Some(k), Some(p)) => (k.trim().to_string(), p.trim().to_string()),
            (Some(_), None) => {
                return Err(format!(
                    "{API_KEY_VAR} is set but {PROJECT_ID_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {PROJECT_ID_VAR}, or unset {API_KEY_VAR} to run \
                     without the platform bridge."
                ))
            }
            (None, Some(_)) => {
                return Err(format!(
                    "{PROJECT_ID_VAR} is set but {API_KEY_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {API_KEY_VAR}, or unset {PROJECT_ID_VAR} to run \
                     without the platform bridge."
                ))
            }
        };

        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_API_URL)
            .trim_end_matches('/')
            .to_string();
        Ok(Some(Self {
            base_url,
            api_key,
            project_id,
        }))
    }

    /// The merged org+project, enabled-only, priority-ordered policy set — the
    /// same endpoint the native gateway and the SDK enforce from. The plain
    /// `/policies` list is project-local, unmerged and includes disabled rows.
    pub fn policies_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/effective",
            self.base_url, self.project_id
        )
    }

    pub fn state_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/state",
            self.base_url, self.project_id
        )
    }

    pub fn admit_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/admit",
            self.base_url, self.project_id
        )
    }

    pub fn reservation_url(&self, id: &str, endpoint: &str) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/reservations/{}/{}",
            self.base_url,
            self.project_id,
            urlencode(id),
            endpoint
        )
    }

    /// Cache identity: a config change (new project, new base URL) must not
    /// keep serving the previous project's compiled policies out of an isolate.
    pub fn cache_key(&self) -> String {
        format!("{}|{}", self.base_url, self.project_id)
    }
}

/// Whether this Worker is configured for a deployment mode it cannot serve.
///
/// Shared tenancy is native-only. The Worker has no tenancy layer: it cannot
/// authenticate a caller against the platform, resolve a credential to the
/// projects it is entitled to, or hold a per-tenant runtime per isolate.
/// [`TENANCY_VAR`] is therefore not a variable it can honor.
///
/// It must not be a variable the Worker *ignores*, either. Every other
/// configuration path in this module refuses rather than degrading, for one
/// reason: an operator who asked for enforcement and silently got a transparent
/// proxy is strictly worse off than one who got an error. Left unread,
/// `NOVEUM_GUARD_TENANCY=shared` on a Worker means every caller's traffic is
/// proxied unguarded while the deployment looks configured.
///
/// Returns the refusal message, or `None` for a configuration the Worker can
/// serve (unset, or an explicit `dedicated`). Pure, so the whole matrix is
/// testable on the native `cargo test` path.
pub fn tenancy_refusal(tenancy: Option<&str>) -> Option<String> {
    // Unset is the historical inference: the credential pair alone decides, and
    // that is dedicated mode, which the Worker does serve.
    let mode = tenancy?.trim();
    if mode.is_empty() {
        return Some(format!(
            "{TENANCY_VAR} is set but empty; an unresolved template or a stripped CI variable is \
             not a deployment mode. Set it to `dedicated`, or unset it."
        ));
    }
    if mode.eq_ignore_ascii_case("dedicated") {
        return None;
    }
    if mode.eq_ignore_ascii_case("shared") {
        return Some(format!(
            "{TENANCY_VAR}=shared is not supported on the Cloudflare Worker. Shared tenancy \
             derives each caller's project and organization from its own Noveum credential, which \
             needs the native gateway's tenancy layer; the Worker can only enforce for one project \
             fixed by {PROJECT_ID_VAR}. Deploy the native gateway for a shared gateway, or set \
             {TENANCY_VAR}=dedicated with {API_KEY_VAR} + {PROJECT_ID_VAR}."
        ));
    }
    Some(format!(
        "{TENANCY_VAR}={mode:?} is not a deployment mode. The Worker supports `dedicated` (one \
         project fixed by {PROJECT_ID_VAR}); `shared` is native-only."
    ))
}

/// Percent-encode the few characters that could break out of a path segment.
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

/// Trim an error body for logging (a CDN/WAF error page can be large HTML).
pub fn truncate_body(body: &str) -> String {
    const MAX: usize = 512;
    let trimmed = body.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &trimmed[..end], trimmed.len())
}

// ---------------------------------------------------------------------------
// Admission wire types
// ---------------------------------------------------------------------------

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
    /// Render as the gateway's uniform [`PolicyDecision`] so a platform block
    /// flows through the same synthetic-response builders as a locally
    /// evaluated one — byte-identical block shapes at the edge and natively.
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
    /// Idempotency key. One per logical request; reused on retry, which is what
    /// makes a retry replay the same reservation instead of reserving twice.
    pub request_id: String,
    pub provider: Option<String>,
    pub model: String,
    pub estimated_input_tokens: u64,
    pub maximum_output_tokens: u64,
    pub estimated_cost_usd: f64,
}

impl AdmitRequest {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
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
    pub fn endpoint(&self) -> &'static str {
        match self {
            Settlement::Complete(_) => "complete",
            Settlement::Abandon(_) => "abandon",
            Settlement::Cancel(_) => "cancel",
        }
    }

    /// The POST body. `timestamp` is injected rather than read from a clock so
    /// this stays pure (and so the wasm build can use `Date.now()` without a
    /// `chrono` clock dependency in the hot path).
    pub fn to_json(&self, timestamp: Option<&str>) -> Value {
        match self {
            Settlement::Complete(u) => {
                let mut v = json!({
                    "inputTokens": u.input_tokens,
                    "outputTokens": u.output_tokens,
                    "costUsd": sanitize_cost(u.cost_usd),
                    "requestCount": u.request_count,
                });
                if let Some(m) = &u.model {
                    v["model"] = Value::from(m.as_str());
                }
                if let Some(e) = &u.event_id {
                    v["eventId"] = Value::from(e.as_str());
                }
                if let Some(t) = timestamp {
                    v["timestamp"] = Value::from(t);
                }
                v
            }
            Settlement::Abandon(reason) | Settlement::Cancel(reason) => json!({ "reason": reason }),
        }
    }
}

/// Classify an `/admit` HTTP response. Pure, so the whole contract — including
/// the counter-intuitive "**blocked is HTTP 200**" — is unit-testable without a
/// platform, which matters double here because the wasm integration path cannot
/// be executed in this environment.
///
/// Anything that is not an unambiguous allow or an unambiguous block is
/// [`Admission::Unavailable`], and the caller then applies `failClosed`. There
/// is deliberately no path from a malformed or unexpected response to "allowed".
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
            // platform's hold would linger for its whole TTL. Treat it as
            // unavailable rather than admitting a call we can never close out.
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

// ---------------------------------------------------------------------------
// Pure request-path decisions
// ---------------------------------------------------------------------------

/// How a request's provider stream (or buffered body) ended, from the settling
/// side's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The body reached EOF and the provider's own token counts were recovered.
    /// A reported `completion_tokens: 0` lands here and is authoritative.
    Usage(ActualUsage),
    /// The body reached EOF but carried no authoritative usage: a stream with
    /// no terminal usage frame, a provider truncation, or an input-only report
    /// (an Anthropic `message_start` from a stream that then died).
    EndedWithoutUsage,
    /// The response body was dropped before EOF — the client disconnected, or
    /// the isolate tore the stream down. The call reached the provider.
    Dropped,
}

/// Choose the settlement for a stream/body outcome.
///
/// The semantics are the native path's, unchanged:
///
/// * authoritative usage → **complete**, reconciling the reservation DOWN from
///   `input + max_tokens` to what actually ran;
/// * anything else → **abandon**, which RETAINS the conservative estimate,
///   because the call may well have reached the provider;
/// * **cancel** is not reachable from here at all. It releases the hold and is
///   only correct when the request provably never reached the provider — the
///   caller issues it directly, from the gateway-side block path.
pub fn settlement_for(outcome: StreamOutcome, model: &str, event_id: Option<String>) -> Settlement {
    match outcome {
        StreamOutcome::Usage(u) => {
            let cost =
                crate::policy::pricing::estimate_cost(model, u.input_tokens, u.output_tokens);
            Settlement::Complete(Box::new(SettlementUsage {
                model: Some(model.to_string()),
                input_tokens: u64::from(u.input_tokens),
                output_tokens: u64::from(u.output_tokens),
                cost_usd: cost,
                request_count: 1,
                event_id,
            }))
        }
        StreamOutcome::EndedWithoutUsage => {
            Settlement::Abandon("response ended without authoritative usage".to_string())
        }
        StreamOutcome::Dropped => {
            Settlement::Abandon("client disconnected before the response completed".to_string())
        }
    }
}

/// Settlement outcome for a **buffered** (non-streaming) response body.
///
/// `converted` is the OpenAI-shaped body the gateway is about to return, or
/// `None` when there is no JSON body at all (a non-JSON payload, or one past the
/// inspection cap). `raw_usage` is whatever the *provider's own* body carried
/// before translation, and is consulted only as a fallback: Bedrock reports
/// `usage.inputTokens`/`outputTokens` in camelCase, a spelling the OpenAI
/// conversion normalizes but a failed conversion would drop.
///
/// No usage at all — an error envelope, a 4xx, a body past the cap — is
/// [`StreamOutcome::EndedWithoutUsage`], i.e. abandon: the call reached the
/// provider and nothing measurable came back, so the estimate has to stand.
pub fn buffered_body_outcome(
    converted: Option<&Value>,
    raw_usage: Option<ActualUsage>,
) -> StreamOutcome {
    match converted
        .and_then(crate::policy::metering::extract_actual_usage)
        .or(raw_usage)
    {
        Some(u) => StreamOutcome::Usage(u),
        None => StreamOutcome::EndedWithoutUsage,
    }
}

/// Estimated input tokens for the admission arithmetic.
///
/// Deliberately the *same* heuristic as the native path
/// (`ProviderMetrics::estimate_tokens_from_text`: ~4 characters per token,
/// rounded up), so a given prompt reserves the same amount at the edge as it
/// does on the native gateway. It is **not**
/// `rules::token_length_cap::estimate_tokens`, whose wasm arm returns the UTF-8
/// byte length: that is a deliberate over-count for a guardrail that must not
/// under-report, and using it here would over-reserve cap headroom ~4x and
/// block ordinary traffic.
pub fn estimate_input_tokens(text: &str) -> u32 {
    let chars = text.chars().count();
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}

/// The request's declared completion budget, or `Err(key)` when the client sent
/// an unusable one. Mirrors `middleware::resolve_max_output_tokens`.
pub fn resolve_max_output_tokens(body: &Value) -> Result<Option<u64>, &'static str> {
    for key in OUTPUT_LIMIT_KEYS {
        let Some(v) = body.get(key) else { continue };
        if v.is_null() {
            continue;
        }
        return match v.as_u64() {
            Some(n) if n <= MAX_OUTPUT_TOKEN_LIMIT => Ok(Some(n)),
            _ => Err(key),
        };
    }
    Ok(None)
}

/// Force `stream_options.include_usage` on an OpenAI streaming request while
/// platform metering is active. Returns `true` when the body was changed.
///
/// Without it a default OpenAI stream carries no token counts at all, the
/// reservation can never be reconciled, and every streamed request settles at
/// its `input + max_tokens` estimate — routinely ~100x the real cost. The flag
/// is FORCED, not defaulted: honoring an explicit `include_usage: false` would
/// let any caller select approximate accounting for their own spend.
pub fn force_include_usage(provider: &str, body: &mut Value) -> bool {
    if !provider.eq_ignore_ascii_case("openai") {
        return false;
    }
    if body.get("stream").and_then(|s| s.as_bool()) != Some(true) {
        return false;
    }
    if body
        .pointer("/stream_options/include_usage")
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return false;
    }
    // Replace a missing OR malformed `stream_options` wholesale — indexing into
    // a non-object would panic.
    match body.get_mut("stream_options").filter(|v| v.is_object()) {
        Some(opts) => opts["include_usage"] = json!(true),
        None => body["stream_options"] = json!({"include_usage": true}),
    }
    true
}

/// What to do with an incoming request body given its declared `Content-Length`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyAdmission {
    /// Read it, still enforcing the cap while reading.
    Read,
    /// Reject with `413` before reading a single byte.
    TooLarge,
}

/// Decide from the declared `Content-Length` whether to read the request body.
///
/// A *declared* length over the cap is rejected up front — reading 200 MB into
/// a 128 MB isolate only to reject it is how a Worker OOMs. An absent,
/// malformed or chunked length is not a licence to skip the cap: it returns
/// [`BodyAdmission::Read`], and the reader enforces the same bound
/// incrementally.
pub fn admit_request_body(content_length: Option<&str>, cap: usize) -> BodyAdmission {
    match content_length.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) if n > cap as u64 => BodyAdmission::TooLarge,
        _ => BodyAdmission::Read,
    }
}

// ---------------------------------------------------------------------------
// wasm32 I/O — worker::Fetch. No policy decisions live below this line.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod wasm_io {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use futures_util::future::{select, Either};
    use worker::{
        console_error, console_warn, js_sys, Delay, Fetch, Headers, Method, Request, RequestInit,
        Result,
    };

    use crate::policy::config::PolicyBundle;
    use crate::policy::engine::{EngineOptions, PolicyEngine};
    use crate::policy::platform;
    use crate::policy::rules::LiveState;

    /// How long a compiled policy set is reused inside one isolate before it is
    /// revalidated with `If-None-Match`. The platform serves
    /// `Cache-Control: max-age=30`; a 304 costs almost nothing.
    const POLICY_TTL_MS: f64 = 60_000.0;
    /// How long a live-state snapshot is reused. `/state` is itself cached ~30s
    /// server-side.
    const STATE_TTL_MS: f64 = 10_000.0;
    /// After a failed control-plane fetch, don't retry from this isolate for a
    /// while — an outage must not add a connect timeout to every request.
    const ERROR_BACKOFF_MS: f64 = 5_000.0;
    /// Hard bound on an in-request admission call. Exceeding it is
    /// [`Admission::Unavailable`] — **never** an implicit allow.
    const ADMIT_BUDGET: Duration = Duration::from_secs(2);
    /// Hard bound on an in-request `/state` refresh.
    const STATE_BUDGET: Duration = Duration::from_secs(2);
    /// Attempts for one admission call. Retrying the same `requestId` replays
    /// the same reservation, so this cannot double-reserve.
    const ADMIT_ATTEMPTS: u32 = 2;
    const ADMIT_RETRY_DELAY: Duration = Duration::from_millis(50);
    /// Attempts for a settlement call. Settlement runs under `wait_until`, off
    /// the client's path, and every endpoint is idempotent per reservation.
    const SETTLE_ATTEMPTS: u32 = 3;
    const SETTLE_BASE_DELAY: Duration = Duration::from_millis(250);

    /// One HTTP exchange with the control plane.
    struct HttpOutcome {
        status: u16,
        body: Vec<u8>,
        etag: Option<String>,
    }

    fn now_ms() -> f64 {
        js_sys::Date::now()
    }

    fn now_iso() -> String {
        js_sys::Date::new_0().to_iso_string().into()
    }

    struct CachedPolicies {
        key: String,
        engine: Rc<PolicyEngine>,
        etag: Option<String>,
        fetched_ms: f64,
        last_error_ms: Option<f64>,
    }

    struct CachedState {
        key: String,
        state: Option<LiveState>,
        etag: Option<String>,
        fetched_ms: f64,
        last_error_ms: Option<f64>,
    }

    // The Workers runtime is single-threaded per isolate, so a thread-local is
    // the whole of the concurrency story. Every borrow below is released before
    // an `await`; nothing holds a `RefCell` guard across a suspension point.
    thread_local! {
        static POLICY_CACHE: RefCell<Option<CachedPolicies>> = const { RefCell::new(None) };
        static STATE_CACHE: RefCell<Option<CachedState>> = const { RefCell::new(None) };
    }

    /// Race `fut` against a timer. `None` means the budget was exceeded.
    async fn with_budget<T>(
        budget: Duration,
        fut: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        futures_util::pin_mut!(fut);
        match select(fut, Delay::from(budget)).await {
            Either::Left((v, _)) => Some(v),
            Either::Right((_, _)) => None,
        }
    }

    /// Issue one authenticated control-plane request over `worker::Fetch`.
    async fn send(
        cfg: &WorkerRemoteConfig,
        method: Method,
        url: &str,
        body: Option<&Value>,
        if_none_match: Option<&str>,
    ) -> Result<HttpOutcome> {
        let headers = Headers::new();
        headers.set("authorization", &format!("Bearer {}", cfg.api_key))?;
        headers.set("accept", "application/json")?;
        if let Some(tag) = if_none_match {
            headers.set("if-none-match", tag)?;
        }
        let mut init = RequestInit::new();
        init.with_method(method).with_headers(headers);
        if let Some(b) = body {
            let text = serde_json::to_string(b).unwrap_or_else(|_| "{}".to_string());
            init.headers.set("content-type", "application/json")?;
            init.with_body(Some(js_sys::JsString::from(text.as_str()).into()));
        }
        let req = Request::new_with_init(url, &init)?;
        let mut resp = Fetch::Request(req).send().await?;
        let status = resp.status_code();
        let etag = resp.headers().get("etag").ok().flatten();
        // A 304 carries no body; `bytes()` on it is an empty vec, not an error.
        let body = resp.bytes().await.unwrap_or_default();
        Ok(HttpOutcome { status, body, etag })
    }

    /// The compiled effective policy set for this project, revalidated at most
    /// once per [`POLICY_TTL_MS`] per isolate.
    ///
    /// A fetch *error* keeps the last known good set (a transient platform blip
    /// must never wipe enforcement). A *successful empty* set is applied — the
    /// platform is the source of truth and a project may legitimately have zero
    /// policies. With no cached set at all, the error propagates and the caller
    /// refuses the request, unless the operator opted into an unguarded start.
    pub async fn effective_engine(
        cfg: &WorkerRemoteConfig,
        opts: EngineOptions,
    ) -> core::result::Result<Rc<PolicyEngine>, String> {
        let key = cfg.cache_key();
        let now = now_ms();

        // Snapshot what we know, then drop the borrow before any await.
        let (cached, etag, fresh, backing_off) = POLICY_CACHE.with(|c| {
            let c = c.borrow();
            match c.as_ref().filter(|p| p.key == key) {
                Some(p) => (
                    Some(p.engine.clone()),
                    p.etag.clone(),
                    now - p.fetched_ms < POLICY_TTL_MS,
                    p.last_error_ms.is_some_and(|t| now - t < ERROR_BACKOFF_MS),
                ),
                None => (None, None, false, false),
            }
        });
        if let Some(engine) = &cached {
            if fresh || backing_off {
                return Ok(engine.clone());
            }
        }

        let outcome = send(
            cfg,
            Method::Get,
            &cfg.policies_url(),
            None,
            etag.as_deref().filter(|_| cached.is_some()),
        )
        .await
        .map_err(|e| format!("policies fetch failed: {e}"));

        let parsed: core::result::Result<Option<(PolicyBundle, Option<String>)>, String> =
            match outcome {
                Err(e) => Err(e),
                Ok(o) if o.status == 304 => Ok(None),
                Ok(o) if !(200..300).contains(&o.status) => Err(format!(
                    "policies fetch returned {}: {}",
                    o.status,
                    truncate_body(&String::from_utf8_lossy(&o.body))
                )),
                Ok(o) => match serde_json::from_slice::<Value>(&o.body) {
                    Err(e) => Err(format!("policies fetch returned invalid JSON: {e}")),
                    Ok(v) => platform::translate_bundle(&v)
                        .map(|b| Some((b, o.etag)))
                        .map_err(|e| format!("policies fetch returned an unusable bundle: {e}")),
                },
            };

        match parsed {
            // 304: the cached set is still current; reset its freshness clock.
            Ok(None) => {
                let engine = cached.ok_or_else(|| {
                    "policies fetch returned 304 with no cached policy set".to_string()
                })?;
                POLICY_CACHE.with(|c| {
                    if let Some(p) = c.borrow_mut().as_mut().filter(|p| p.key == key) {
                        p.fetched_ms = now_ms();
                        p.last_error_ms = None;
                    }
                });
                Ok(engine)
            }
            Ok(Some((bundle, new_etag))) => {
                let engine = Rc::new(PolicyEngine::from_bundle(&bundle, opts));
                for rejected in engine.rejected_policies() {
                    console_error!("Nova Guard: platform policy rejected: {rejected}");
                }
                POLICY_CACHE.with(|c| {
                    *c.borrow_mut() = Some(CachedPolicies {
                        key: key.clone(),
                        engine: engine.clone(),
                        etag: new_etag,
                        fetched_ms: now_ms(),
                        last_error_ms: None,
                    });
                });
                Ok(engine)
            }
            Err(e) => {
                POLICY_CACHE.with(|c| {
                    if let Some(p) = c.borrow_mut().as_mut().filter(|p| p.key == key) {
                        p.last_error_ms = Some(now_ms());
                    }
                });
                match cached {
                    // Keep enforcing what we last knew; say so loudly.
                    Some(engine) => {
                        console_warn!(
                            "Nova Guard: platform policy refresh failed ({e}); keeping the last known policy set"
                        );
                        Ok(engine)
                    }
                    None => Err(e),
                }
            }
        }
    }

    /// The live cost/rate counters, revalidated at most once per
    /// [`STATE_TTL_MS`] per isolate.
    ///
    /// `None` means *unavailable*, which the engine turns into a fail-closed
    /// block or a fail-open allow per policy. An expired snapshot that cannot
    /// be revalidated is deliberately NOT served: an arbitrarily old under-cap
    /// snapshot would let a `/state` outage silently defeat fail-closed
    /// enforcement after warm-up.
    pub async fn live_state(cfg: &WorkerRemoteConfig) -> Option<LiveState> {
        let key = cfg.cache_key();
        let now = now_ms();

        let (cached, etag, fresh, backing_off) = STATE_CACHE.with(|c| {
            let c = c.borrow();
            match c.as_ref().filter(|s| s.key == key) {
                Some(s) => (
                    s.state.clone(),
                    s.etag.clone(),
                    now - s.fetched_ms < STATE_TTL_MS,
                    s.last_error_ms.is_some_and(|t| now - t < ERROR_BACKOFF_MS),
                ),
                None => (None, None, false, false),
            }
        });
        if fresh {
            return cached;
        }
        if backing_off {
            return None;
        }

        let fetched = with_budget(
            STATE_BUDGET,
            send(
                cfg,
                Method::Get,
                &cfg.state_url(),
                None,
                etag.as_deref().filter(|_| cached.is_some()),
            ),
        )
        .await;

        let outcome = match fetched {
            None => Err("state fetch exceeded the in-request refresh budget".to_string()),
            Some(Err(e)) => Err(format!("state fetch failed: {e}")),
            Some(Ok(o)) if o.status == 304 => Ok(None),
            Some(Ok(o)) if !(200..300).contains(&o.status) => Err(format!(
                "state fetch returned {}: {}",
                o.status,
                truncate_body(&String::from_utf8_lossy(&o.body))
            )),
            Some(Ok(o)) => match serde_json::from_slice::<Value>(&o.body) {
                Err(e) => Err(format!("state fetch returned invalid JSON: {e}")),
                Ok(v) => {
                    if v.get("stale").and_then(|s| s.as_bool()) == Some(true) {
                        console_warn!("Nova Guard: live-state served stale (durable fallback)");
                    }
                    Ok(Some((platform::state_to_live_state(&v), o.etag)))
                }
            },
        };

        match outcome {
            Ok(None) => {
                STATE_CACHE.with(|c| {
                    if let Some(s) = c.borrow_mut().as_mut().filter(|s| s.key == key) {
                        s.fetched_ms = now_ms();
                        s.last_error_ms = None;
                    }
                });
                cached
            }
            Ok(Some((state, new_etag))) => {
                STATE_CACHE.with(|c| {
                    *c.borrow_mut() = Some(CachedState {
                        key: key.clone(),
                        state: Some(state.clone()),
                        etag: new_etag,
                        fetched_ms: now_ms(),
                        last_error_ms: None,
                    });
                });
                Some(state)
            }
            Err(e) => {
                console_warn!("Nova Guard: {e}; treating live state as unavailable");
                STATE_CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    match c.as_mut().filter(|s| s.key == key) {
                        Some(s) => {
                            s.state = None;
                            s.last_error_ms = Some(now_ms());
                        }
                        None => {
                            *c = Some(CachedState {
                                key: key.clone(),
                                state: None,
                                etag: None,
                                fetched_ms: 0.0,
                                last_error_ms: Some(now_ms()),
                            })
                        }
                    }
                });
                None
            }
        }
    }

    /// Reserve this request's estimated usage atomically, platform-side.
    ///
    /// Bounded by [`ADMIT_BUDGET`]; exceeding it is [`Admission::Unavailable`],
    /// never an allow.
    pub async fn admit(cfg: &WorkerRemoteConfig, req: &AdmitRequest) -> Admission {
        match with_budget(ADMIT_BUDGET, admit_inner(cfg, req)).await {
            Some(outcome) => outcome,
            None => Admission::Unavailable(format!(
                "admission exceeded the {}ms request budget",
                ADMIT_BUDGET.as_millis()
            )),
        }
    }

    async fn admit_inner(cfg: &WorkerRemoteConfig, req: &AdmitRequest) -> Admission {
        let url = cfg.admit_url();
        let body = req.to_json();
        let mut last_error: Option<String> = None;
        for attempt in 1..=ADMIT_ATTEMPTS {
            match send(cfg, Method::Post, &url, Some(&body), None).await {
                Ok(o) => {
                    // A gateway-level blip (5xx other than 503, or a 429) is
                    // worth one more try: the same `requestId` replays. A **503**
                    // is not retried — that is the platform explicitly saying
                    // admission is unevaluable, and hammering it only adds
                    // latency to an outage.
                    let retryable =
                        o.status != 503 && (o.status == 429 || (500..600).contains(&o.status));
                    if retryable && attempt < ADMIT_ATTEMPTS {
                        last_error = Some(format!("admit returned {}", o.status));
                        Delay::from(ADMIT_RETRY_DELAY).await;
                        continue;
                    }
                    return classify_admit(o.status, &o.body);
                }
                Err(e) => {
                    last_error = Some(e.to_string());
                    if attempt < ADMIT_ATTEMPTS {
                        Delay::from(ADMIT_RETRY_DELAY).await;
                    }
                }
            }
        }
        Admission::Unavailable(format!(
            "admit failed after {ADMIT_ATTEMPTS} attempts: {}",
            last_error.unwrap_or_else(|| "unknown transport error".to_string())
        ))
    }

    /// Close out a reservation. Idempotent per reservation id; retries transient
    /// failures. Always invoked from `ctx.wait_until(..)`, never on the client's
    /// critical path.
    pub async fn settle(
        cfg: &WorkerRemoteConfig,
        reservation_id: &str,
        settlement: &Settlement,
    ) -> bool {
        let url = cfg.reservation_url(reservation_id, settlement.endpoint());
        let iso = now_iso();
        let body = settlement.to_json(Some(&iso));
        for attempt in 1..=SETTLE_ATTEMPTS {
            let transient = match send(cfg, Method::Post, &url, Some(&body), None).await {
                Ok(o) if (200..300).contains(&o.status) => return true,
                Ok(o) if o.status == 429 || (500..600).contains(&o.status) => true,
                Ok(o) => {
                    console_error!(
                        "Nova Guard: reservation {reservation_id} {} rejected with {}: {}; giving up",
                        settlement.endpoint(),
                        o.status,
                        truncate_body(&String::from_utf8_lossy(&o.body))
                    );
                    return false;
                }
                Err(e) => {
                    console_warn!(
                        "Nova Guard: reservation {reservation_id} {} attempt {attempt} failed: {e}",
                        settlement.endpoint()
                    );
                    true
                }
            };
            if transient && attempt < SETTLE_ATTEMPTS {
                let mult = 1u32 << (attempt - 1).min(16);
                Delay::from(SETTLE_BASE_DELAY * mult).await;
            }
        }
        console_error!(
            "Nova Guard: reservation {reservation_id} {} gave up after {SETTLE_ATTEMPTS} attempts; \
             the platform hold stands until it expires",
            settlement.endpoint()
        );
        false
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm_io::{admit, effective_engine, live_state, settle};

#[cfg(test)]
mod tests {
    use super::*;

    // -- configuration matrix -------------------------------------------------

    #[test]
    fn credential_matrix_matches_the_native_bridge() {
        // Neither set: the bridge is intentionally off.
        assert_eq!(WorkerRemoteConfig::from_values(None, None, None), Ok(None));

        // Both set: configured, with the default base URL and trimmed values.
        let cfg = WorkerRemoteConfig::from_values(Some(" k "), Some(" proj_1 "), None)
            .unwrap()
            .unwrap();
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.project_id, "proj_1");
        assert_eq!(cfg.base_url, DEFAULT_API_URL);

        // A trailing slash on the override never doubles up in a URL.
        let cfg =
            WorkerRemoteConfig::from_values(Some("k"), Some("p"), Some("https://api.test.dev/"))
                .unwrap()
                .unwrap();
        assert_eq!(cfg.base_url, "https://api.test.dev");
    }

    #[test]
    fn a_half_applied_or_blank_credential_pair_is_a_configuration_error() {
        for (key, project, needle) in [
            (Some("k"), None, "NOVEUM_GUARD_PROJECT_ID is not"),
            (None, Some("p"), "NOVEUM_API_KEY is not"),
            (Some(""), Some("p"), "NOVEUM_API_KEY is set but empty"),
            (Some("   "), Some("p"), "NOVEUM_API_KEY is set but empty"),
            (
                Some("k"),
                Some(""),
                "NOVEUM_GUARD_PROJECT_ID is set but empty",
            ),
        ] {
            let e = WorkerRemoteConfig::from_values(key, project, None)
                .expect_err("a half-applied bridge must not be treated as 'disabled'");
            assert!(e.contains(needle), "unhelpful message: {e}");
        }
    }

    /// The Worker cannot serve shared tenancy, so it must REFUSE it rather than
    /// read past it. Ignoring the variable is the one outcome that leaves an
    /// operator believing per-tenant caps are live on an unguarded proxy.
    #[test]
    fn shared_tenancy_is_refused_on_the_worker_not_ignored() {
        // Serveable: unset (the historical credential-pair inference) and an
        // explicit dedicated, in any casing or padding.
        assert_eq!(tenancy_refusal(None), None);
        for ok in ["dedicated", "DEDICATED", "  Dedicated  "] {
            assert_eq!(tenancy_refusal(Some(ok)), None, "{ok:?} must be serveable");
        }

        // Shared: refused, naming the mode, the reason and the way out.
        let m = tenancy_refusal(Some("shared")).expect("shared must be refused on the Worker");
        assert!(m.contains("not supported on the Cloudflare Worker"), "{m}");
        assert!(m.contains(PROJECT_ID_VAR), "the way out must be named: {m}");
        assert!(
            tenancy_refusal(Some(" SHARED ")).is_some(),
            "casing and padding must not smuggle shared mode past the check"
        );

        // A blank value is an unresolved template, not "unset" — the same class
        // of failure `from_values` refuses for the credential pair.
        for blank in ["", "   "] {
            let m = tenancy_refusal(Some(blank))
                .unwrap_or_else(|| panic!("{blank:?} must not be read as unset"));
            assert!(m.contains("set but empty"), "{m}");
        }

        // A typo must not silently fall through to dedicated.
        let m = tenancy_refusal(Some("dedicted")).expect("a typo must be refused");
        assert!(m.contains("not a deployment mode"), "{m}");
    }

    fn cfg() -> WorkerRemoteConfig {
        WorkerRemoteConfig {
            base_url: "https://api.example.com".into(),
            api_key: "k".into(),
            project_id: "proj_1".into(),
        }
    }

    #[test]
    fn urls_match_the_documented_contract() {
        let c = cfg();
        assert_eq!(
            c.policies_url(),
            "https://api.example.com/api/v1/projects/proj_1/policies/effective"
        );
        assert_eq!(
            c.state_url(),
            "https://api.example.com/api/v1/projects/proj_1/policies/state"
        );
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
    fn a_config_change_invalidates_the_isolate_cache_key() {
        let a = cfg();
        let mut b = cfg();
        b.project_id = "proj_2".into();
        assert_ne!(a.cache_key(), b.cache_key());
    }

    // -- admit classification -------------------------------------------------

    #[test]
    fn allowed_response_yields_a_settleable_reservation() {
        let body = br#"{"allowed":true,"reservationId":"res-1","expiresAt":"2026-08-12T00:00:00Z",
             "policyVersion":"\"etag\"","replayed":false,"shadowed":[]}"#;
        match classify_admit(200, body) {
            Admission::Allowed(r) => {
                assert_eq!(r.id, "res-1");
                assert_eq!(r.expires_at.as_deref(), Some("2026-08-12T00:00:00Z"));
                assert_eq!(r.policy_version.as_deref(), Some("\"etag\""));
                assert!(!r.replayed);
            }
            other => panic!("expected Allowed, got {other:?}"),
        }
        // A retried requestId replays rather than double-reserving.
        match classify_admit(
            200,
            br#"{"allowed":true,"reservationId":"res-1","replayed":true}"#,
        ) {
            Admission::Allowed(r) => assert!(r.replayed),
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
                assert_eq!(d.projected, Some(1200.5));
                let pd = d.to_policy_decision();
                assert!(pd.is_blocking(), "a platform block must block at the edge");
                assert_eq!(pd.reason, "org 7d cap reached");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_blocks_map_to_rate_limit_and_a_bare_block_is_still_a_block() {
        let body = br#"{"allowed":false,"decision":{"policyId":"p","policyName":"n",
            "policyType":"RATE_LIMIT","reason":"too many"}}"#;
        match classify_admit(200, body) {
            Admission::Blocked(d) => assert_eq!(d.policy_type, "rate_limit"),
            other => panic!("expected Blocked, got {other:?}"),
        }
        match classify_admit(200, br#"{"allowed":false}"#) {
            Admission::Blocked(d) => {
                assert_eq!(d.policy_type, "cost_cap");
                assert!(!d.reason.is_empty());
                assert!(d.to_policy_decision().is_blocking());
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
        let Admission::Unavailable(r) =
            classify_admit(503, br#"{"message":"GUARDRAIL_ADMISSION_UNAVAILABLE"}"#)
        else {
            panic!("expected Unavailable");
        };
        assert!(
            r.contains("GUARDRAIL_ADMISSION_UNAVAILABLE"),
            "the platform code must reach the operator: {r}"
        );
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
        assert_eq!(v["estimatedInputTokens"], 1200);
        assert_eq!(v["maximumOutputTokens"], 4096);
        assert_eq!(v["estimatedCostUsd"], 0.0051552);

        let v = AdmitRequest {
            request_id: "req-2".into(),
            provider: None,
            model: "m".into(),
            estimated_input_tokens: 0,
            maximum_output_tokens: 0,
            estimated_cost_usd: f64::NAN,
        }
        .to_json();
        assert!(v.get("provider").is_none(), "provider is optional");
        assert_eq!(v["estimatedCostUsd"], 0.0, "NaN must not become null");
    }

    // -- settlement -----------------------------------------------------------

    #[test]
    fn settlement_bodies_and_endpoints_match_the_contract() {
        let c = Settlement::Complete(Box::new(SettlementUsage {
            model: Some("gpt-4o".into()),
            input_tokens: 123,
            output_tokens: 45,
            cost_usd: 0.0031,
            request_count: 1,
            event_id: Some("evt-1".into()),
        }));
        assert_eq!(c.endpoint(), "complete");
        let v = c.to_json(Some("2026-08-12T00:00:00Z"));
        assert_eq!(v["inputTokens"], 123);
        assert_eq!(v["outputTokens"], 45);
        assert_eq!(v["costUsd"], 0.0031);
        assert_eq!(v["requestCount"], 1);
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["eventId"], "evt-1");
        assert_eq!(v["timestamp"], "2026-08-12T00:00:00Z");
        // The timestamp is optional server-side; omitting it is still valid.
        assert!(c.to_json(None).get("timestamp").is_none());

        let a = Settlement::Abandon("stream ended without usage".into());
        assert_eq!(a.endpoint(), "abandon");
        assert_eq!(a.to_json(None)["reason"], "stream ended without usage");

        let x = Settlement::Cancel("blocked before dispatch".into());
        assert_eq!(x.endpoint(), "cancel");
        assert_eq!(x.to_json(None)["reason"], "blocked before dispatch");
    }

    #[test]
    fn authoritative_usage_completes_and_reconciles_the_cost() {
        let s = settlement_for(
            StreamOutcome::Usage(ActualUsage {
                input_tokens: 11,
                output_tokens: 4,
            }),
            "gpt-4o",
            Some("evt-1".into()),
        );
        let Settlement::Complete(u) = &s else {
            panic!("authoritative usage must complete, got {s:?}");
        };
        assert_eq!(s.endpoint(), "complete");
        assert_eq!(u.input_tokens, 11);
        assert_eq!(u.output_tokens, 4);
        assert_eq!(u.request_count, 1);
        assert_eq!(u.model.as_deref(), Some("gpt-4o"));
        assert!(u.cost_usd > 0.0, "a priced model must yield a real cost");
    }

    /// A provider that explicitly reported zero completion tokens made a real
    /// measurement. Abandoning it would retain `input + max_tokens` — routinely
    /// ~100x — for a request that generated nothing.
    #[test]
    fn a_reported_zero_output_is_authoritative_and_completes() {
        let s = settlement_for(
            StreamOutcome::Usage(ActualUsage {
                input_tokens: 7,
                output_tokens: 0,
            }),
            "gpt-4o",
            None,
        );
        assert_eq!(s.endpoint(), "complete");
        let Settlement::Complete(u) = &s else {
            unreachable!()
        };
        assert_eq!(u.output_tokens, 0);
        assert!(u.event_id.is_none());
    }

    /// EOF without usage, provider truncation, an input-only report and a client
    /// disconnect all mean the same thing: the call may have reached the
    /// provider and no measurement came back, so the conservative estimate has
    /// to stand. Never `cancel` — that would release the hold.
    #[test]
    fn every_non_authoritative_outcome_abandons_and_none_of_them_cancel() {
        for outcome in [StreamOutcome::EndedWithoutUsage, StreamOutcome::Dropped] {
            let s = settlement_for(outcome, "gpt-4o", None);
            assert_eq!(s.endpoint(), "abandon", "{outcome:?} must abandon");
            assert!(
                matches!(&s, Settlement::Abandon(r) if !r.is_empty()),
                "{outcome:?} must carry a reason"
            );
        }
        // The two reasons are distinguishable in the platform's audit trail.
        assert_ne!(
            settlement_for(StreamOutcome::EndedWithoutUsage, "m", None),
            settlement_for(StreamOutcome::Dropped, "m", None)
        );
    }

    /// `StreamUsageScanner` is the single source of "is this authoritative?".
    /// Wire it to `settlement_for` here so the Worker's end-to-end decision —
    /// bytes in, settlement out — is covered natively even though the Worker
    /// itself cannot be executed in this environment.
    #[test]
    fn stream_bytes_drive_the_settlement_decision_end_to_end() {
        use crate::policy::metering::StreamUsageScanner;

        let outcome_of = |raw: &str| -> StreamOutcome {
            let mut s = StreamUsageScanner::new();
            // Feed in tiny slices: transport chunking must not change the answer.
            for piece in raw.as_bytes().chunks(3) {
                s.push(piece);
            }
            s.finish();
            match s.usage() {
                Some(u) => StreamOutcome::Usage(u),
                None => StreamOutcome::EndedWithoutUsage,
            }
        };

        // A real OpenAI stream with its terminal usage chunk → complete.
        let openai = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                      data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
                      data: [DONE]\n\n";
        let s = settlement_for(outcome_of(openai), "gpt-4o", None);
        assert_eq!(s.endpoint(), "complete");
        let Settlement::Complete(u) = &s else {
            unreachable!()
        };
        assert_eq!((u.input_tokens, u.output_tokens), (11, 4));

        // A raw Anthropic stream: usage split across message_start/message_delta.
        let anthropic = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n\
                         event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n";
        let Settlement::Complete(u) =
            settlement_for(outcome_of(anthropic), "claude-sonnet-4-5", None)
        else {
            panic!("an Anthropic stream reports usage and must complete");
        };
        assert_eq!((u.input_tokens, u.output_tokens), (11, 4));

        // A stream with no usage at all (include_usage absent) → abandon.
        let bare = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(
            settlement_for(outcome_of(bare), "gpt-4o", None).endpoint(),
            "abandon"
        );

        // Truncated after message_start: input known, output NOT. Completing at
        // output=0 would release the entire hold.
        let truncated = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n";
        assert_eq!(
            settlement_for(outcome_of(truncated), "claude-sonnet-4-5", None).endpoint(),
            "abandon"
        );
    }

    #[test]
    fn a_buffered_body_settles_from_its_usage_object() {
        use serde_json::json;

        // The ordinary OpenAI shape.
        let body = json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 34}});
        assert_eq!(
            buffered_body_outcome(Some(&body), None),
            StreamOutcome::Usage(ActualUsage {
                input_tokens: 12,
                output_tokens: 34
            })
        );

        // Bedrock's camelCase counters survive only in the pre-translation body;
        // the fallback is what keeps a Converse response meterable.
        let converted = json!({"choices": []});
        let raw = ActualUsage {
            input_tokens: 5,
            output_tokens: 6,
        };
        assert_eq!(
            buffered_body_outcome(Some(&converted), Some(raw)),
            StreamOutcome::Usage(raw)
        );
        // ...but the converted body wins when it has counters of its own.
        let converted = json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2}});
        assert_eq!(
            buffered_body_outcome(Some(&converted), Some(raw)),
            StreamOutcome::Usage(ActualUsage {
                input_tokens: 1,
                output_tokens: 2
            })
        );

        // No body (non-JSON, or past the inspection cap), an error envelope, and
        // an empty `usage` all abandon rather than completing at zero.
        for body in [
            None,
            Some(json!({"error": {"message": "bad request"}})),
            Some(json!({"usage": {}})),
            Some(json!({"usage": null})),
        ] {
            assert_eq!(
                buffered_body_outcome(body.as_ref(), None),
                StreamOutcome::EndedWithoutUsage,
                "body {body:?} must abandon"
            );
            assert_eq!(
                settlement_for(buffered_body_outcome(body.as_ref(), None), "gpt-4o", None)
                    .endpoint(),
                "abandon"
            );
        }
    }

    // -- request-path decisions ----------------------------------------------

    #[test]
    fn the_request_body_cap_rejects_a_declared_oversize_before_reading() {
        const CAP: usize = 8 * 1024 * 1024;
        assert_eq!(
            admit_request_body(Some("8388609"), CAP),
            BodyAdmission::TooLarge
        );
        assert_eq!(
            admit_request_body(Some("8388608"), CAP),
            BodyAdmission::Read
        );
        assert_eq!(admit_request_body(Some("0"), CAP), BodyAdmission::Read);
        assert_eq!(admit_request_body(Some(" 1024 "), CAP), BodyAdmission::Read);
        // Absent / malformed / chunked: not a licence to skip the cap — the
        // reader still enforces it incrementally.
        for header in [None, Some(""), Some("abc"), Some("-1"), Some("12,34")] {
            assert_eq!(
                admit_request_body(header, CAP),
                BodyAdmission::Read,
                "header {header:?} must fall through to the incremental cap"
            );
        }
        // A length that overflows usize on a 32-bit (wasm) target must still be
        // rejected rather than wrapping into an acceptable value.
        assert_eq!(
            admit_request_body(Some("99999999999999"), CAP),
            BodyAdmission::TooLarge
        );
    }

    #[test]
    fn output_limits_are_validated_before_they_reach_the_admission_arithmetic() {
        use serde_json::json;
        assert_eq!(resolve_max_output_tokens(&json!({})), Ok(None));
        assert_eq!(
            resolve_max_output_tokens(&json!({"max_tokens": 512})),
            Ok(Some(512))
        );
        assert_eq!(
            resolve_max_output_tokens(&json!({"max_completion_tokens": 64})),
            Ok(Some(64))
        );
        // A null limit is "unset", not "invalid".
        assert_eq!(
            resolve_max_output_tokens(&json!({"max_tokens": null, "max_output_tokens": 32})),
            Ok(Some(32))
        );
        // `0` is accepted (it is what the native path accepts); only values that
        // are not usable token counts at all are rejected.
        assert_eq!(
            resolve_max_output_tokens(&json!({"max_tokens": 0})),
            Ok(Some(0))
        );
        for bad in [
            json!({"max_tokens": -1}),
            json!({"max_tokens": "many"}),
            json!({"max_tokens": MAX_OUTPUT_TOKEN_LIMIT + 1}),
        ] {
            assert_eq!(
                resolve_max_output_tokens(&bad),
                Err("max_tokens"),
                "body {bad} must be rejected"
            );
        }
    }

    #[test]
    fn include_usage_is_forced_on_openai_streams_only() {
        use serde_json::json;

        // Missing stream_options → created.
        let mut b = json!({"stream": true});
        assert!(force_include_usage("openai", &mut b));
        assert_eq!(b["stream_options"]["include_usage"], true);

        // An explicit `false` is OVERRIDDEN: honoring it would let a caller pick
        // approximate accounting for their own spend.
        let mut b = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert!(force_include_usage("openai", &mut b));
        assert_eq!(b["stream_options"]["include_usage"], true);

        // A malformed stream_options is replaced wholesale, not indexed into.
        let mut b = json!({"stream": true, "stream_options": "nope"});
        assert!(force_include_usage("openai", &mut b));
        assert_eq!(b["stream_options"]["include_usage"], true);

        // Already correct → untouched.
        let mut b = json!({"stream": true, "stream_options": {"include_usage": true}});
        assert!(!force_include_usage("openai", &mut b));

        // Non-streaming, and non-OpenAI providers (Anthropic reports usage in
        // its own message_delta), are left alone.
        let mut b = json!({"stream": false});
        assert!(!force_include_usage("openai", &mut b));
        let mut b = json!({"stream": true});
        assert!(!force_include_usage("anthropic", &mut b));
        assert!(b.get("stream_options").is_none());
    }

    #[test]
    fn input_token_estimate_matches_the_native_heuristic() {
        // ~4 characters per token, rounded UP, counted in chars (not bytes) so a
        // multi-byte prompt reserves the same at the edge as it does natively.
        assert_eq!(estimate_input_tokens(""), 0);
        assert_eq!(estimate_input_tokens("abcd"), 1);
        assert_eq!(estimate_input_tokens("abcde"), 2);
        assert_eq!(estimate_input_tokens("héllo wörld 🌍"), 4);
        let native = |t: &str| (t.chars().count() as f32 / 4.0).ceil() as u32;
        for text in ["", "a", "hello there", "héllo wörld 🌍", &"x".repeat(4097)] {
            assert_eq!(estimate_input_tokens(text), native(text), "text: {text:?}");
        }
    }

    #[test]
    fn oversized_error_bodies_are_truncated_on_a_char_boundary() {
        let body = "é".repeat(1000);
        let out = truncate_body(&body);
        assert!(out.len() < body.len());
        assert!(out.contains("bytes)"));
        assert_eq!(truncate_body("  hi  "), "hi");
    }
}
