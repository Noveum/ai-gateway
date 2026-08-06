//! Nova Guard Tower middleware.
//!
//! Runs the policy engine on the request (input phase) before the upstream
//! provider call, and on the response (output phase) after. Blocks short-circuit
//! with a provider-shaped synthetic response; transforms rewrite the payload.
//!
//! Fast path: when the engine is disabled or has zero active policies, the
//! middleware returns immediately without buffering the body, so guardrails add
//! no overhead when unused.
//!
//! Streaming responses (`text/event-stream`) are passed through without
//! output-phase enforcement in v1 (a documented limitation shared across LLM
//! gateways); input-phase enforcement and blocking still apply to streaming
//! requests.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, Request},
    response::Response,
};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::decision::Phase;
use super::engine::PolicyEngine;
use super::synthetic::block_response;

/// Max request/response body we will buffer for inspection (8 MiB).
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Largest output limit we will accept in a request body. The biggest documented
/// provider completion ceiling is ~128K tokens, so this leaves two orders of
/// magnitude of headroom while keeping the admission arithmetic (input tokens +
/// output limit, both `u64`) far away from overflow. `max_tokens` is untrusted
/// client JSON: an out-of-range value used to panic the request task in debug
/// builds and silently wrap the token reservation in release builds, so anything
/// above this is rejected with a deterministic 400 instead.
const MAX_OUTPUT_TOKEN_LIMIT: u64 = 10_000_000;

/// Keys a request may use to bound its completion length, in precedence order.
const OUTPUT_LIMIT_KEYS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

/// Read the request's output limit, rejecting values that are not usable token
/// counts. `Ok(None)` means "unbounded" (no key present, or explicitly `null`);
/// `Err(key)` names the offending field so the caller can return a 400.
fn resolve_max_output_tokens(body_json: &Value) -> Result<Option<u64>, &'static str> {
    for key in OUTPUT_LIMIT_KEYS {
        let Some(v) = body_json.get(key) else {
            continue;
        };
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

/// Provider-shaped 400 for an unusable output limit.
fn invalid_output_limit_response(key: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": format!(
                "`{key}` must be a positive integer no greater than {MAX_OUTPUT_TOKEN_LIMIT}"
            ),
            "type": "invalid_request_error",
            "param": key,
            "code": "invalid_value",
        }
    });
    Response::builder()
        .status(axum::http::StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response is valid")
}

/// State threaded into [`guard_middleware`]: the engine plus an optional provider
/// of platform live cost/rate state (for `cost_cap`/`rate_limit`). `live` is
/// `None` when platform-managed Nova Guard isn't configured.
#[derive(Clone)]
pub struct GuardState {
    pub engine: Arc<PolicyEngine>,
    pub live: Option<Arc<crate::policy::remote::RemoteLiveState>>,
    /// Reports BLOCKED usage events to the platform. `None` when platform-managed
    /// Nova Guard (and thus usage reporting) isn't configured.
    pub usage: Option<crate::policy::usage::UsageReporter>,
    /// In-process ledger of estimated spend for requests already admitted but not
    /// yet reflected in the platform counters (usage posts asynchronously,
    /// `/state` is cached). Added on top of the platform spend when evaluating
    /// cost caps so a burst near the cap can't all slip through.
    pub pending: Arc<crate::policy::remote::PendingSpend>,
}

pub async fn guard_middleware(
    State(gs): State<GuardState>,
    req: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let engine = gs.engine.clone();
    // Fast path: nothing to enforce.
    if !engine.is_enabled() || engine.active_policy_count() == 0 {
        return next.run(req).await;
    }

    // Only inspect JSON POST bodies on the proxy path.
    if !is_guardable(&req) {
        return next.run(req).await;
    }

    let provider = header_str(&req, "x-provider").unwrap_or_else(|| "openai".to_string());

    // --- INPUT PHASE ---
    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            // Body too large or unreadable; we cannot inspect it. Fail open by
            // forwarding the original (already-consumed) request is impossible,
            // so respond with a clear error rather than silently dropping.
            return Response::builder()
                .status(axum::http::StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::from("request body exceeds gateway inspection limit"))
                .unwrap();
        }
    };

    let json: Option<Value> = serde_json::from_slice(&bytes).ok();
    let model = json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // Live cost/rate counters from the platform (for cost_cap/rate_limit). `None`
    // when platform-managed Nova Guard isn't configured → those policies fail open.
    let mut live_state = match &gs.live {
        Some(remote) => remote.get().await,
        None => None,
    };

    let mut forward_bytes = bytes.clone();
    let mut body_mutated = false;
    // This request's pending-usage reservation. Held as an RAII guard from the
    // moment it is taken, so that a cancellation *anywhere* below — including
    // while awaiting upstream response headers — still transitions the ledger
    // entry out of ACTIVE instead of leaking it until the 15-minute backstop.
    let mut reservation: Option<crate::policy::remote::ReservationGuard> = None;

    if let Some(mut body_json) = json.clone() {
        let input_text = flatten_input_text(&body_json);
        let est_input_tokens =
            crate::telemetry::provider_metrics::ProviderMetrics::estimate_tokens_from_text(
                &input_text,
            );
        // Untrusted client JSON — reject an unusable limit before it reaches the
        // admission arithmetic (or the provider).
        let max_output_tokens = match resolve_max_output_tokens(&body_json) {
            Ok(v) => v,
            Err(key) => {
                warn!(provider = %provider, model = %model, field = key,
                      "Nova Guard: rejecting request with an out-of-range output limit");
                return invalid_output_limit_response(key);
            }
        };
        let est_request_cost = crate::policy::pricing::estimate_request_cost(
            &model,
            est_input_tokens,
            max_output_tokens,
        )
        .unwrap_or(0.0);
        // `max_output_tokens` is bounded by `MAX_OUTPUT_TOKEN_LIMIT` above, so
        // this cannot overflow; `saturating_add` keeps that true if either
        // bound ever changes.
        let est_tokens = u64::from(est_input_tokens).saturating_add(
            max_output_tokens.unwrap_or_else(crate::policy::pricing::assumed_output_tokens),
        );

        // Reserve this request's predicted usage (cost + one request + tokens)
        // and read the other in-flight reservations in ONE critical section —
        // reserving *before* evaluating means two concurrent requests always
        // see each other's reservation, so a burst near an almost-exhausted
        // cost cap OR rate limit can't all pass on the same stale counters.
        // Blocked requests release the reservation below.
        if gs.usage.is_some() {
            let (res, others) = gs.pending.reserve(est_request_cost, est_tokens);
            // Scope the guard to this future immediately: from here on, every
            // exit path — block, panic, or the whole middleware future being
            // dropped mid-flight — transitions the entry out of ACTIVE.
            reservation = Some(crate::policy::remote::ReservationGuard::new(
                gs.pending.clone(),
                res,
            ));
            if let Some(ls) = live_state.as_mut() {
                if others.cost_usd > 0.0 {
                    for v in ls.cost_usd_by_window.values_mut() {
                        *v += others.cost_usd;
                    }
                    for v in ls.org_cost_usd_by_window.values_mut() {
                        *v += others.cost_usd;
                    }
                }
                if others.requests > 0 {
                    for v in ls.requests_by_window.values_mut() {
                        *v += others.requests;
                    }
                    for v in ls.org_requests_by_window.values_mut() {
                        *v += others.requests;
                    }
                }
                if others.tokens > 0 {
                    for v in ls.tokens_by_window.values_mut() {
                        *v += others.tokens;
                    }
                    for v in ls.org_tokens_by_window.values_mut() {
                        *v += others.tokens;
                    }
                }
            }
        }

        let result = engine.evaluate(
            Phase::Input,
            &model,
            &input_text,
            Some(&body_json),
            Some(est_input_tokens),
            live_state.as_ref(),
        );

        log_decisions("input", &provider, &model, &result.decisions);

        if let Some(block) = &result.block {
            // A blocked request never reaches the provider — disarm the guard
            // and drop the reservation entirely so it stops counting against
            // the cap immediately (rather than lingering for the post-
            // completion TTL, which is for calls that really were forwarded).
            if let Some(guard) = reservation.take() {
                guard.release();
            }
            info!(
                provider = %provider, model = %model, policy = %block.policy_id,
                reason = %block.reason, "Nova Guard blocked request (input phase)"
            );
            // Report a BLOCKED usage event for cost_cap/rate_limit blocks. The
            // platform only accepts those as usage blocks (text-rule blocks like
            // PII/regex map to no `blockedBy`, so they're not reported). Sent in
            // place of the model call; the platform doesn't meter it and fires the
            // owner "limit hit" email.
            //
            // Require live state to have been present: only a block backed by real
            // counters is a genuine limit breach. A fail-closed block caused by an
            // unreachable `/state` is *not* reported — it isn't a "limit hit" and
            // must not trigger the owner email (matches the SDK).
            if live_state.is_some() {
                if let Some(reporter) = &gs.usage {
                    if let Some(blocked_by) =
                        crate::policy::usage::blocked_by_for(&block.policy_type)
                    {
                        reporter.report(crate::policy::usage::UsageEvent::blocked(
                            crate::policy::usage::new_event_id(),
                            model.clone(),
                            blocked_by,
                            Some(block.policy_id.clone()),
                            Some(block.reason.clone()),
                        ));
                    }
                }
            }
            return block_response(&provider, &model, block, engine.block_mode());
        }

        // Apply input transforms per text segment (so structured messages stay valid).
        if result.transformed_text.is_some()
            && apply_input_transforms(&engine, &model, &mut body_json)
        {
            if let Ok(v) = serde_json::to_vec(&body_json) {
                forward_bytes = v.into();
                body_mutated = true;
            }
        }

        // When the platform is metering usage, ask OpenAI to append the final
        // usage chunk to streaming responses (`stream_options.include_usage`).
        // Without it a default stream carries no token counts, the ALLOWED event
        // posts $0, and cost caps silently never advance. The extra final chunk
        // is standard OpenAI shape (empty `choices` + `usage`) that SDKs handle.
        //
        // The flag is FORCED, not merely defaulted: an explicit
        // `include_usage: false` is overridden while platform metering is
        // active. Honoring it would let any caller select approximate
        // accounting for their own spend — a real Luna stream that reports
        // 11 in / 4 out ($0.000035) falls back to 6 in / 1 out ($0.000012),
        // a 65.7% undercount, and reasoning tokens are not recoverable from
        // visible deltas at all. The delta-based estimator stays what it was
        // meant to be: an outage fallback for streams that arrive without
        // usage, not a client-selectable accounting mode.
        if gs.usage.is_some()
            && provider.eq_ignore_ascii_case("openai")
            && body_json.get("stream").and_then(|s| s.as_bool()) == Some(true)
            && body_json
                .pointer("/stream_options/include_usage")
                .and_then(|v| v.as_bool())
                != Some(true)
        {
            debug!(
                model = %model,
                overridden = body_json
                    .pointer("/stream_options/include_usage")
                    .is_some(),
                "Nova Guard: forcing stream_options.include_usage for platform metering"
            );
            // Replace a missing OR malformed `stream_options` wholesale —
            // indexing into a non-object would panic.
            match body_json
                .get_mut("stream_options")
                .filter(|v| v.is_object())
            {
                Some(opts) => opts["include_usage"] = serde_json::json!(true),
                None => body_json["stream_options"] = serde_json::json!({"include_usage": true}),
            }
            if let Ok(v) = serde_json::to_vec(&body_json) {
                forward_bytes = v.into();
                body_mutated = true;
            }
        }
    }

    let mut parts = parts;
    if body_mutated {
        // The body length changed; drop the stale Content-Length so the
        // downstream layer/provider recomputes it (avoids truncation/hang).
        parts.headers.remove(header::CONTENT_LENGTH);
    }
    let forwarded = Request::from_parts(parts, Body::from(forward_bytes));
    let response = next.run(forwarded).await;

    // --- OUTPUT PHASE ---
    let response = enforce_output(&engine, &provider, &model, response, live_state.as_ref()).await;

    // Transfer the guard we have held since `reserve()` into the response body,
    // so the reservation stays ACTIVE until the body finishes (or the client
    // disconnects): a long-lived or streaming request never loses its cap
    // protection mid-flight, and the post-completion TTL starts only once usage
    // reporting can actually begin. Note this is a *move*, not a new guard —
    // had the future been dropped before reaching here (client gone while we
    // awaited upstream headers), the same guard would already have completed
    // the reservation on the way out.
    match reservation {
        Some(guard) => attach_reservation_guard(response, guard),
        None => response,
    }
}

/// Wrap the response body so `guard` is dropped exactly when the body has been
/// fully streamed to the client — or the connection is dropped — marking the
/// reservation completed. The bytes pass through untouched.
fn attach_reservation_guard(
    response: Response,
    guard: crate::policy::remote::ReservationGuard,
) -> Response {
    use futures_util::StreamExt;
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _keep_alive = &guard;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Should this request be inspected? POST, JSON, on the `/v1/` proxy path.
fn is_guardable<B>(req: &Request<B>) -> bool {
    if req.method() != axum::http::Method::POST {
        return false;
    }
    // Anchor to the proxy route prefix so unrelated JSON POSTs that merely
    // contain "/v1/" elsewhere in the path are not buffered/scanned.
    if !req.uri().path().starts_with("/v1/") {
        return false;
    }
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("application/json"))
        .unwrap_or(false)
}

fn header_str<B>(req: &Request<B>, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// Request/response shaping is shared with the Cloudflare Worker so input/output
// scanning + transforms are byte-identical on both deployment shapes.
pub use crate::routing::{
    apply_input_transforms, apply_output_transforms, flatten_input_text, flatten_output_text,
};

async fn enforce_output(
    engine: &PolicyEngine,
    provider: &str,
    model: &str,
    response: Response,
    live_state: Option<&crate::policy::rules::LiveState>,
) -> Response {
    // Skip streaming responses (documented v1 limitation).
    let is_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);
    if is_stream {
        debug!("Nova Guard: streaming response passed through without output enforcement (v1)");
        return response;
    }

    // If the response declares a length beyond our inspection cap, pass it
    // through untouched rather than buffering it (avoids corrupting large
    // legitimate completions and avoids an OOM vector).
    let declared_len = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    if matches!(declared_len, Some(n) if n > MAX_BODY) {
        debug!("Nova Guard: response exceeds inspection cap; passing through unchanged");
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            // Chunked response with no declared length exceeded the cap mid-read;
            // the original body is no longer recoverable. Return an honest error
            // envelope with a correct status rather than a 200 carrying a string.
            warn!("Nova Guard: chunked response exceeded inspection cap; returning 502");
            return Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"error":{"message":"upstream response exceeded gateway inspection limit","type":"gateway_error"}}"#,
                ))
                .expect("static error response is valid");
        }
    };

    let json: Option<Value> = serde_json::from_slice(&bytes).ok();
    let Some(body_json) = json else {
        // Not JSON (or empty) — pass through unchanged.
        return Response::from_parts(parts, Body::from(bytes));
    };

    // Native providers (notably Anthropic) convert the upstream body to OpenAI
    // chat-completion shape BEFORE this middleware runs. Pick the flatten/transform
    // shape from the ACTUAL body, not the `x-provider` name, so output enforcement
    // never silently misses `choices[].message.content`.
    // `x-provider` is case-insensitive; normalize so e.g. "Gemini" still matches
    // the `candidates` shape instead of silently failing open.
    let provider_key = provider.to_ascii_lowercase();
    let output_provider = if body_json.get("choices").is_some() {
        "openai"
    } else {
        provider_key.as_str()
    };

    let output_text = flatten_output_text(output_provider, &body_json);
    if output_text.is_empty() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let result = engine.evaluate(
        Phase::Output,
        model,
        &output_text,
        Some(&body_json),
        None,
        live_state,
    );
    log_decisions("output", provider, model, &result.decisions);

    if let Some(block) = &result.block {
        info!(
            provider = %provider, model = %model, policy = %block.policy_id,
            "Nova Guard blocked response (output phase)"
        );
        return block_response(provider, model, block, engine.block_mode());
    }

    // Output transform: redact EVERY assistant text segment in place (all
    // choices / content blocks), not just the first, so multi-choice responses
    // can't leak. Re-runs the transform per segment via the shared helper.
    if result.transformed_text.is_some() {
        let mut out_json = body_json;
        if apply_output_transforms(engine, model, output_provider, &mut out_json) {
            if let Ok(v) = serde_json::to_vec(&out_json) {
                let mut parts = parts;
                parts.headers.remove(header::CONTENT_LENGTH);
                return Response::from_parts(parts, Body::from(v));
            }
        }
    }

    Response::from_parts(parts, Body::from(bytes))
}

fn log_decisions(
    phase: &str,
    provider: &str,
    model: &str,
    decisions: &[super::decision::PolicyDecision],
) {
    for d in decisions {
        if d.flagged {
            debug!(
                phase = phase, provider = provider, model = model,
                policy = %d.policy_id, policy_type = %d.policy_type,
                action = ?d.action, mode = ?d.mode, severity = ?d.severity,
                "Nova Guard policy decision"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_openai_messages() {
        let j = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "hello there"}
            ]
        });
        let t = flatten_input_text(&j);
        assert!(t.contains("be helpful"));
        assert!(t.contains("hello there"));
    }

    #[test]
    fn flattens_anthropic_system_and_parts() {
        let j = serde_json::json!({
            "model": "claude-opus-4-8",
            "system": "you are terse",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "summarize"}]}
            ]
        });
        let t = flatten_input_text(&j);
        assert!(t.contains("you are terse"));
        assert!(t.contains("summarize"));
    }

    #[test]
    fn flattens_plain_prompt() {
        let j = serde_json::json!({"prompt": "once upon a time"});
        assert!(flatten_input_text(&j).contains("once upon a time"));
    }

    #[test]
    fn extracts_openai_output() {
        let j = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "the answer is 42"}}]
        });
        assert_eq!(flatten_output_text("openai", &j), "the answer is 42");
    }

    #[test]
    fn extracts_anthropic_output() {
        let j = serde_json::json!({"content": [{"type": "text", "text": "hi"}]});
        assert_eq!(flatten_output_text("anthropic", &j), "hi");
    }

    #[test]
    fn extracts_google_output() {
        let j = serde_json::json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "g-out"}]}}]
        });
        assert_eq!(flatten_output_text("google", &j), "g-out");
    }

    #[test]
    fn apply_output_transforms_redacts_all_openai_choices() {
        // Output-phase email redaction over a multi-choice response: every choice
        // must be rewritten (regression guard — the old helper did only the first).
        let bundle = crate::policy::config::PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"red","type":"pii_detection","mode":"enforce","config":{"phase":"output","entities":["EMAIL_ADDRESS"],"action":"redact"}}]}"#,
        )
        .unwrap();
        let e = PolicyEngine::from_bundle(&bundle, crate::policy::engine::EngineOptions::default());
        let mut j = serde_json::json!({
            "choices": [
                {"message": {"role": "assistant", "content": "ping a@b.com"}},
                {"message": {"role": "assistant", "content": "or c@d.io"}}
            ]
        });
        assert!(apply_output_transforms(&e, "gpt-4o", "openai", &mut j));
        assert_eq!(j["choices"][0]["message"]["content"], "ping [REDACTED]");
        assert_eq!(j["choices"][1]["message"]["content"], "or [REDACTED]");
    }

    fn redact_email_engine() -> PolicyEngine {
        let bundle = crate::policy::config::PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"red","type":"pii_detection","mode":"enforce",
            "config":{"phase":"input","entities":["EMAIL_ADDRESS"],"action":"redact"}}]}"#,
        )
        .unwrap();
        PolicyEngine::from_bundle(&bundle, crate::policy::engine::EngineOptions::default())
    }

    #[test]
    fn apply_input_transforms_redacts_string_content() {
        let e = redact_email_engine();
        let mut j = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "mail me at a@b.com"}]
        });
        assert!(apply_input_transforms(&e, "gpt-4o", &mut j));
        assert_eq!(j["messages"][0]["content"], "mail me at [REDACTED]");
    }

    #[test]
    fn apply_input_transforms_redacts_array_multimodal_parts() {
        // H2: array-form (multimodal) content parts must be redacted, not just scanned.
        let e = redact_email_engine();
        let mut j = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "reach me: a@b.com"},
                    {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
                    {"type": "text", "text": "or c@d.io"}
                ]
            }]
        });
        assert!(apply_input_transforms(&e, "gpt-4o", &mut j));
        assert_eq!(
            j["messages"][0]["content"][0]["text"],
            "reach me: [REDACTED]"
        );
        assert_eq!(j["messages"][0]["content"][2]["text"], "or [REDACTED]");
        // Non-text part is untouched.
        assert_eq!(
            j["messages"][0]["content"][1]["image_url"]["url"],
            "https://x/y.png"
        );
    }

    #[test]
    fn apply_input_transforms_redacts_array_system_blocks() {
        // H2: Anthropic array-form `system` blocks must also be redacted.
        let e = redact_email_engine();
        let mut j = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": "owner is admin@corp.com"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(apply_input_transforms(&e, "claude-sonnet-4-5", &mut j));
        assert_eq!(j["system"][0]["text"], "owner is [REDACTED]");
    }

    #[test]
    fn apply_input_transforms_noop_without_match() {
        let e = redact_email_engine();
        let mut j = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "no pii here"}]
        });
        assert!(!apply_input_transforms(&e, "gpt-4o", &mut j));
    }

    #[test]
    fn is_guardable_only_for_post_json_v1() {
        use axum::http::{header, Method, Request};
        let mk = |method: Method, path: &str, ct: Option<&str>| {
            let mut b = Request::builder().method(method).uri(path);
            if let Some(ct) = ct {
                b = b.header(header::CONTENT_TYPE, ct);
            }
            b.body(()).unwrap()
        };
        assert!(is_guardable(&mk(
            Method::POST,
            "/v1/chat/completions",
            Some("application/json")
        )));
        // wrong method
        assert!(!is_guardable(&mk(
            Method::GET,
            "/v1/chat/completions",
            Some("application/json")
        )));
        // not anchored to /v1/
        assert!(!is_guardable(&mk(
            Method::POST,
            "/health",
            Some("application/json")
        )));
        assert!(!is_guardable(&mk(
            Method::POST,
            "/api/v1/x",
            Some("application/json")
        )));
        // non-json
        assert!(!is_guardable(&mk(
            Method::POST,
            "/v1/chat/completions",
            Some("text/plain")
        )));
        assert!(!is_guardable(&mk(
            Method::POST,
            "/v1/chat/completions",
            None
        )));
    }
}
