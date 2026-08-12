//! The wire contract for the platform's atomic admission API, shared by both
//! deployment shapes.
//!
//! The gateway talks to `.../policies/admit` and
//! `.../policies/reservations/{id}/{complete|abandon|cancel}` from two places:
//! [`crate::policy::admission`] on native, over `reqwest`, and
//! [`crate::policy::worker_remote`] on `wasm32`, over `worker::Fetch`. Only the
//! transport differs. Everything that *decides* something is here:
//!
//! * the request and settlement bodies, and their exact camelCase field names;
//! * [`classify_admit`], including the contract's sharpest edge — a **block is
//!   HTTP 200**, and a **503 is never an allow**;
//! * how a `200 {allowed:false}` decision becomes a gateway [`PolicyDecision`],
//!   so a platform block renders byte-identically at the edge and natively;
//! * URL-segment encoding and error-body truncation.
//!
//! These halves used to be duplicated, on the theory that tests on both sides
//! would catch a divergence. That only ever detects a divergence *someone wrote
//! a test for*: the failure it cannot catch is one copy being updated and the
//! other not, which is silent until an edge deployment bills differently from a
//! native one. There is now one copy.
//!
//! The module is deliberately free of `reqwest`, `tokio` and `chrono` so it
//! compiles for `wasm32`. That is also why [`Settlement::to_json`] takes its
//! timestamp as an argument instead of reading a clock: the native side passes
//! `chrono`, the Worker passes `Date.now()`, and the function stays pure and
//! testable on either.

use serde_json::{json, Value};

use crate::policy::decision::{PolicyAction, PolicyDecision, PolicyMode, Severity};

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
    /// Render as the gateway's uniform [`PolicyDecision`], so a platform block
    /// flows through the same synthetic-response, logging and telemetry paths as
    /// a locally evaluated one.
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
    /// Idempotency key. One per logical request; reused on retry, which is what
    /// makes a retry replay the same reservation instead of reserving twice.
    pub request_id: String,
    pub provider: Option<String>,
    pub model: String,
    pub estimated_input_tokens: u64,
    pub maximum_output_tokens: u64,
    pub estimated_cost_usd: f64,
    /// Version of the pricing catalog `estimated_cost_usd` was computed with.
    ///
    /// Rates change, and they change on a schedule the gateway applies without a
    /// deploy. Without this, a hold is an amount with no way to reproduce how it
    /// was arrived at, and a reservation taken under last week's catalog is
    /// indistinguishable from one taken under this week's. Usage records already
    /// carry it (`usage.rs`), so a reservation omitting it was the one gap in
    /// the audit trail between estimating a cost and settling it.
    pub pricing_version: Option<String>,
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
        if let Some(pv) = &self.pricing_version {
            v["pricingVersion"] = Value::from(pv.as_str());
        }
        v
    }
}

/// Coerce an estimated cost into the platform's accepted range (`number >= 0`),
/// mapping NaN to 0 rather than emitting a `null` the schema rejects.
pub fn sanitize_cost(v: f64) -> f64 {
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
    /// Pricing catalog version behind `cost_usd`. See
    /// [`AdmitRequest::pricing_version`]. A reservation and its settlement can
    /// legitimately disagree here, when a scheduled rate change lands between
    /// the two, which is precisely why both carry it rather than one.
    pub pricing_version: Option<String>,
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
    /// this stays pure, and so the wasm build can pass `Date.now()` without
    /// pulling a `chrono` clock into the hot path.
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
                if let Some(pv) = &u.pricing_version {
                    v["pricingVersion"] = Value::from(pv.as_str());
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
/// platform, which matters double for the Worker, whose transport cannot be
/// executed on the test host.
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
            // platform's hold would linger for its whole TTL. That is a
            // platform-side bug; treat it as unavailable rather than admitting
            // a call we can never close out.
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

/// Percent-encode the few characters that could break out of a path segment. A
/// reservation id is a server-issued UUID, so this guards against a future id
/// format rather than a live concern.
pub fn urlencode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
                assert_eq!(d.observed, Some(1199.5));
                assert_eq!(d.projected, Some(1200.5));
                let pd = d.to_policy_decision();
                assert!(pd.is_blocking(), "a platform block must block");
                assert_eq!(pd.reason, "org 7d cap reached");
                assert_eq!(pd.policy_id, "pol_1");
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
        // An unrecognized type falls back to cost_cap, and a decision-less
        // block still blocks with generic labels.
        for body in [
            &br#"{"allowed":false,"decision":{"policyType":"WHAT"}}"#[..],
            &br#"{"allowed":false}"#[..],
        ] {
            match classify_admit(200, body) {
                Admission::Blocked(d) => {
                    assert_eq!(d.policy_type, "cost_cap");
                    assert!(!d.reason.is_empty());
                    assert!(d.to_policy_decision().is_blocking());
                }
                other => panic!("expected Blocked, got {other:?}"),
            }
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
            pricing_version: Some("2026.08.12".into()),
        }
        .to_json();
        assert_eq!(v["requestId"], "req-1");
        assert_eq!(v["provider"], "openai");
        assert_eq!(v["model"], "gpt-5.6-luna");
        assert_eq!(v["estimatedInputTokens"], 1200);
        assert_eq!(v["maximumOutputTokens"], 4096);
        assert_eq!(v["estimatedCostUsd"], 0.0051552);
        assert_eq!(
            v["pricingVersion"], "2026.08.12",
            "a hold must record the catalog its estimate was priced with"
        );

        let v = AdmitRequest {
            request_id: "req-2".into(),
            provider: None,
            model: "m".into(),
            estimated_input_tokens: 0,
            maximum_output_tokens: 0,
            estimated_cost_usd: f64::NAN,
            pricing_version: None,
        }
        .to_json();
        assert!(v.get("provider").is_none(), "provider is optional");
        assert!(
            v.get("pricingVersion").is_none(),
            "an absent pricing version is OMITTED, never serialized as null"
        );
        assert_eq!(v["estimatedCostUsd"], 0.0, "NaN must not become null");
    }

    #[test]
    fn negative_and_absurd_costs_are_clamped_into_the_accepted_range() {
        assert_eq!(sanitize_cost(-1.0), 0.0);
        assert_eq!(sanitize_cost(f64::NAN), 0.0);
        assert_eq!(sanitize_cost(f64::INFINITY), 0.0);
        assert_eq!(sanitize_cost(1e9), 100_000.0);
        assert_eq!(sanitize_cost(0.5), 0.5);
    }

    #[test]
    fn settlement_bodies_and_endpoints_match_the_contract() {
        let c = Settlement::Complete(Box::new(SettlementUsage {
            model: Some("gpt-4o".into()),
            input_tokens: 123,
            output_tokens: 45,
            cost_usd: 0.0031,
            request_count: 1,
            event_id: Some("evt-1".into()),
            pricing_version: Some("2026.08.12".into()),
        }));
        assert_eq!(c.endpoint(), "complete");
        let v = c.to_json(Some("2026-08-12T00:00:00Z"));
        assert_eq!(v["inputTokens"], 123);
        assert_eq!(v["outputTokens"], 45);
        assert_eq!(v["costUsd"], 0.0031);
        assert_eq!(v["requestCount"], 1);
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["eventId"], "evt-1");
        assert_eq!(v["pricingVersion"], "2026.08.12");
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
    fn a_path_hostile_reservation_id_cannot_escape_its_segment() {
        assert_eq!(urlencode("res-9"), "res-9");
        assert_eq!(urlencode("../../evil"), "..%2F..%2Fevil");
        assert_eq!(urlencode("a b"), "a%20b");
        // Multi-byte input is encoded per UTF-8 byte, not per char.
        assert_eq!(urlencode("é"), "%C3%A9");
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
