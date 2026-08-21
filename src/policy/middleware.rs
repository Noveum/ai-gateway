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
//!
//! Streams are, however, **metered**: a strict-mode stream's body is teed
//! through [`crate::policy::metering::StreamUsageScanner`] as it flows to the
//! client, so the platform reservation settles on the provider's real token
//! counts instead of retaining the `input + max_tokens` estimate. See
//! [`attach_stream_settlement`].

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, Request},
    response::{IntoResponse, Response},
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
            Some(n) if n > 0 && n <= MAX_OUTPUT_TOKEN_LIMIT => Ok(Some(n)),
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

/// Provider-shaped 400 for a request whose completion is unbounded while a
/// strict cost cap is active. Reserving the configured fallback would only be
/// an estimate: the provider could emit more and move spend beyond the cap.
fn missing_strict_output_limit_response() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "a strict Nova Guard cost cap requires an explicit output limit (`max_tokens`, `max_completion_tokens`, or `max_output_tokens`)",
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "missing_output_limit",
        }
    });
    Response::builder()
        .status(axum::http::StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response is valid")
}

fn invalid_strict_input_response(message: impl Into<String>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message.into(),
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "unsupported_strict_input",
        }
    });
    Response::builder()
        .status(axum::http::StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response is valid")
}

/// Provider-shaped 400 for a request that an active cost/rate policy cannot
/// admit or meter. Passing an opaque body through would make the request
/// invisible to the authoritative counters and turn the policy into a bypass.
fn invalid_stateful_input_response(message: impl Into<String>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message.into(),
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "unsupported_stateful_input",
        }
    });
    Response::builder()
        .status(axum::http::StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response is valid")
}

fn transformed_body_too_large_response() -> Response {
    let body = serde_json::json!({
        "error": {
            "message": format!("transformed request body exceeds the {MAX_BODY}-byte gateway limit"),
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "request_too_large",
        }
    });
    Response::builder()
        .status(axum::http::StatusCode::PAYLOAD_TOO_LARGE)
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
    ///
    /// **Advisory mode only.** It is per-process, so N replicas enforce a cap N
    /// times over; strict mode uses [`admission`](crate::policy::admission)
    /// instead. Still the right tool when there is no platform bridge at all,
    /// and when a cap is explicitly advisory.
    pub pending: Arc<crate::policy::remote::PendingSpend>,
    /// Client for the platform's atomic admission API. `Some` whenever the
    /// platform bridge is configured; whether a given request actually uses it
    /// is decided per request by
    /// [`AdmissionClient::strict_for`](crate::policy::admission::AdmissionClient::strict_for).
    pub admission: Option<Arc<crate::policy::admission::AdmissionClient>>,
}

pub async fn guard_middleware(
    State(gs): State<GuardState>,
    req: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    // Shared mode: `tenant_middleware` has already authenticated the caller and
    // put THIS tenant's engine/counters/ledger/reporter in the extensions. Use
    // them in preference to the process-wide state, which in shared mode is an
    // empty no-op engine that must never enforce or meter anything.
    let tenant_scoped = req.extensions().get::<GuardState>().cloned();
    // In shared mode the process-wide telemetry usage exporter does not exist
    // (there is no process-wide tenant to report as), so ALLOWED events are
    // reported from here, against the derived tenant's own reporter.
    let meter_allowed_here = tenant_scoped.is_some();
    let gs = tenant_scoped.unwrap_or(gs);
    let engine = gs.engine.clone();
    // Fast path: nothing to enforce.
    if !engine.is_enabled() || engine.active_policy_count() == 0 {
        return next.run(req).await;
    }

    let provider = header_str(&req, "x-provider").unwrap_or_else(|| "openai".to_string());
    if crate::routing::resolve_provider(&provider).is_none()
        && !provider.eq_ignore_ascii_case("anthropic")
        && !provider.eq_ignore_ascii_case("bedrock")
    {
        return crate::error::AppError::UnsupportedProvider.into_response();
    }
    let admission_mode_override = gs
        .admission
        .as_ref()
        .and_then(|client| client.mode_override());
    let strict_input_required = engine.requires_bounded_json_input(admission_mode_override);
    let stateful_input_required = engine.stateful_policy_count() > 0;

    // An opaque POST cannot prove its model scope or bound provider-side input.
    // More generally, no stateful cost/rate policy can atomically count an
    // opaque request. Passing it through would make multipart/binary traffic an
    // unlimited counter bypass.
    if is_proxy_post(&req) && stateful_input_required && !is_guardable(&req) {
        return if strict_input_required {
            invalid_strict_input_response(
                "a strict Nova Guard cost cap supports only JSON /v1/chat/completions requests",
            )
        } else {
            invalid_stateful_input_response(
                "Nova Guard cost/rate policies support only JSON /v1 requests because opaque bodies cannot be admitted and metered safely",
            )
        };
    }

    // Only inspect JSON POST bodies on the proxy path.
    if !is_guardable(&req) {
        return next.run(req).await;
    }

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
    if stateful_input_required && json.is_none() {
        return if strict_input_required {
            invalid_strict_input_response(
                "a strict Nova Guard cost cap requires a valid JSON request body",
            )
        } else {
            invalid_stateful_input_response(
                "Nova Guard cost/rate policies require a valid JSON request body for admission and metering",
            )
        };
    }
    let model = json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    if stateful_input_required && model.trim().is_empty() {
        return if strict_input_required {
            invalid_strict_input_response(
                "a strict Nova Guard cost cap requires a non-empty model so policy scope and pricing can be resolved",
            )
        } else {
            invalid_stateful_input_response(
                "Nova Guard cost/rate policies require a non-empty model for admission and metering",
            )
        };
    }

    // Provider-local validation must happen before atomic admission. Once a
    // reservation exists, an early 4xx would otherwise look indistinguishable
    // from an uncertain provider failure and conservatively retain the hold,
    // even though these errors prove no upstream call was attempted.
    if engine.stateful_policy_count() > 0 && provider.eq_ignore_ascii_case("anthropic") {
        let bearer_key = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(crate::routing::authorization_bearer_token);
        let native_key = parts
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if bearer_key.or(native_key).is_none() {
            return crate::error::AppError::MissingApiKey.into_response();
        }
        if let Some(body_json) = &json {
            if let Err(message) = crate::routing::openai_to_anthropic_messages(body_json.clone()) {
                return crate::error::AppError::RequestError(message).into_response();
            }
        }
    } else if engine.stateful_policy_count() > 0 && provider.eq_ignore_ascii_case("bedrock") {
        let access = parts
            .headers
            .get("x-aws-access-key-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let secret = parts
            .headers
            .get("x-aws-secret-access-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if access.is_none() || secret.is_none() {
            return crate::error::AppError::RequestError(
                "Bedrock requires x-aws-access-key-id and x-aws-secret-access-key headers"
                    .to_string(),
            )
            .into_response();
        }
    } else if engine.stateful_policy_count() > 0 {
        let bearer = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(crate::routing::authorization_bearer_token);
        if bearer.is_none() {
            return crate::error::AppError::MissingApiKey.into_response();
        }
    }

    // Live cost/rate counters from the platform (for cost_cap/rate_limit). `None`
    // when platform-managed Nova Guard isn't configured → those policies fail open.
    let mut live_state = match &gs.live {
        Some(remote) => remote.get().await,
        None => None,
    };

    let mut forward_bytes = bytes.clone();
    let mut body_mutated = false;
    // This request's forward estimate, retained past the input phase for the
    // strict fail-open metering path at the bottom of this function.
    let mut estimate: Option<RequestEstimate> = None;
    // This request's pending-usage reservation. Held as an RAII guard from the
    // moment it is taken, so that a cancellation *anywhere* below — including
    // while awaiting upstream response headers — still transitions the ledger
    // entry out of ACTIVE instead of leaking it until the 15-minute backstop.
    let mut reservation: Option<crate::policy::remote::ReservationGuard> = None;
    // The platform-side reservation, in strict mode. Same RAII discipline, but
    // the hold lives in the control plane, so it is shared by every replica.
    let mut admission_guard: Option<crate::policy::admission::AdmissionGuard> = None;

    // Strict (cross-replica) or advisory (per-process) cost enforcement for this
    // request? Decided per request because both the policy set and the
    // deployment-wide override can change under a running gateway.
    let strict_client = gs
        .admission
        .as_ref()
        .filter(|a| a.strict_for(engine.has_strict_cost_cap()));
    let strict_cost_cap = strict_client.is_some_and(|client| {
        engine.requires_explicit_output_limit(&model, client.mode_override())
    });

    if let Some(body_json) = json.clone() {
        let input_text = flatten_input_text(&body_json);
        // Build the exact candidate that will be forwarded before estimating
        // it. Policy replacements and forced stream usage can expand the body;
        // strict admission must reserve the post-transform shape, not the
        // caller's smaller original.
        let mut forward_body = body_json.clone();
        apply_input_transforms(&engine, &model, &mut forward_body);
        if gs.usage.is_some()
            && crate::policy::worker_remote::force_include_usage(&provider, &mut forward_body)
        {
            debug!(
                model = %model,
                "Nova Guard: forcing stream_options.include_usage for platform metering"
            );
        }
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
        if strict_cost_cap && max_output_tokens.is_none() {
            warn!(
                provider = %provider,
                model = %model,
                "Nova Guard: rejecting an unbounded request under a strict cost cap"
            );
            return missing_strict_output_limit_response();
        }
        let est_input_tokens = if strict_cost_cap {
            if provider.eq_ignore_ascii_case("openai") {
                let override_base = std::env::var(crate::routing::OPENAI_BASE_URL_VAR).ok();
                if let Err(message) =
                    crate::routing::validate_strict_openai_base_url(override_base.as_deref())
                {
                    return invalid_strict_input_response(message);
                }
            } else if provider.eq_ignore_ascii_case("anthropic") {
                let override_base = std::env::var(crate::routing::ANTHROPIC_BASE_URL_VAR).ok();
                if let Err(message) =
                    crate::routing::validate_strict_anthropic_base_url(override_base.as_deref())
                {
                    return invalid_strict_input_response(message);
                }
            }
            match crate::routing::prepare_strict_admission_body(
                &provider,
                parts.uri.path(),
                &mut forward_body,
            ) {
                Ok(tokens) => tokens,
                Err(message) => return invalid_strict_input_response(message),
            }
        } else {
            crate::routing::estimate_admission_input_tokens(&forward_body, false)
        };
        let serialized_forward = match serde_json::to_vec(&forward_body) {
            Ok(body) => body,
            Err(error) => {
                return invalid_strict_input_response(format!(
                    "request body could not be serialized after Nova Guard transforms: {error}"
                ))
            }
        };
        if serialized_forward.len() > MAX_BODY {
            return transformed_body_too_large_response();
        }
        if serialized_forward.as_slice() != bytes.as_ref() {
            forward_bytes = serialized_forward.into();
            body_mutated = true;
        }
        // Reserve every billable dimension the request declares up front. An
        // unknown model still takes the defensive catalog maximum; a cache
        // miss or paid search cannot walk past a cap that later settlement
        // correctly charges.
        let declared_usage = crate::policy::pricing::declared_request_usage(
            &provider,
            &forward_body,
            est_input_tokens,
        );
        let est_request_cost = crate::policy::pricing::reserve_request_breakdown(
            &model,
            est_input_tokens,
            max_output_tokens,
            &declared_usage,
        )
        .total_usd;
        // `max_output_tokens` is bounded by `MAX_OUTPUT_TOKEN_LIMIT` above, so
        // this cannot overflow; `saturating_add` keeps that true if either
        // bound ever changes.
        let est_output_tokens =
            max_output_tokens.unwrap_or_else(crate::policy::pricing::assumed_output_tokens);
        let est_tokens = u64::from(est_input_tokens).saturating_add(est_output_tokens);
        estimate = Some(RequestEstimate {
            input_tokens: est_input_tokens,
            output_tokens: u32::try_from(est_output_tokens).unwrap_or(u32::MAX),
            cost_usd: est_request_cost,
        });

        // --- ADMISSION ---
        //
        // STRICT: the platform reserves atomically against ONE counter shared by
        // every replica, and its answer is final. The local ledger is
        // deliberately NOT applied on top — that would count this request's
        // estimate twice.
        //
        // ADVISORY (or no platform bridge): reserve in the per-process ledger
        // and fold the *other* in-flight reservations into the counters the
        // engine evaluates. Race-free within one process, multiplied by replica
        // count across a fleet — which is exactly why strict mode exists.
        if let Some(client) = strict_client {
            let admit = crate::policy::admission::AdmitRequest {
                // Fresh idempotency key per logical request. Retries inside the
                // client reuse it, so a transport failure replays the same
                // reservation instead of reserving twice.
                request_id: crate::policy::usage::new_event_id(),
                provider: Some(provider.clone()),
                model: model.clone(),
                estimated_input_tokens: u64::from(est_input_tokens),
                maximum_output_tokens: max_output_tokens
                    .unwrap_or_else(crate::policy::pricing::assumed_output_tokens),
                estimated_cost_usd: est_request_cost,
                // The catalog the estimate above came from. Rates change on a
                // schedule the gateway applies with no deploy, so without this
                // a hold cannot be reproduced after the fact.
                pricing_version: Some(crate::policy::pricing::CATALOG_VERSION.to_string()),
                force_strict_cost_caps: matches!(
                    client.mode_override(),
                    Some(crate::policy::config::CostEnforcementMode::Strict)
                ),
            };
            match client.admit(&admit).await {
                crate::policy::admission::Admission::Allowed(res) => {
                    // Scope the guard immediately: from here on every exit path
                    // settles the reservation (see `AdmissionGuard`).
                    admission_guard = Some(crate::policy::admission::AdmissionGuard::new(
                        client.clone(),
                        res.id,
                    ));
                }
                crate::policy::admission::Admission::Blocked(decision) => {
                    let block = decision.to_policy_decision();
                    info!(
                        provider = %provider, model = %model, policy = %block.policy_id,
                        scope = ?decision.scope, dimension = ?decision.dimension,
                        limit = ?decision.limit, projected = ?decision.projected,
                        reason = %block.reason,
                        "Nova Guard blocked request (platform admission)"
                    );
                    // No BLOCKED usage event here: the platform *is* what
                    // blocked this call, so it already holds the record. Posting
                    // one to `/usage` would double-report the limit hit (and
                    // re-fire the owner email).
                    return block_response(&provider, &model, &block, engine.block_mode());
                }
                crate::policy::admission::Admission::Unavailable(reason) => {
                    // Never an implicit allow. Apply `failClosed` exactly as an
                    // unavailable `/state` would.
                    let force_strict_cost_caps = matches!(
                        client.mode_override(),
                        Some(crate::policy::config::CostEnforcementMode::Strict)
                    );
                    match engine.admission_unavailable_decision(
                        &reason,
                        &model,
                        force_strict_cost_caps,
                    ) {
                        Some(d) if d.is_blocking() => {
                            warn!(
                                provider = %provider, model = %model, policy = %d.policy_id,
                                reason = %d.reason,
                                "Nova Guard: platform admission unavailable; failing closed"
                            );
                            return block_response(&provider, &model, &d, engine.block_mode());
                        }
                        Some(d) => warn!(
                            provider = %provider, model = %model, policy = %d.policy_id,
                            reason = %d.reason,
                            "Nova Guard: platform admission unavailable; failing open"
                        ),
                        None => warn!(
                            provider = %provider, model = %model, reason = %reason,
                            "Nova Guard: platform admission unavailable and no applicable stateful policy was routed through admission"
                        ),
                    }
                }
            }
        } else if gs.usage.is_some() {
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
            // Same for a platform reservation: the call provably never reached
            // the provider, so `cancel` (which releases the hold) is correct
            // here — and is the ONLY place it is.
            if let Some(guard) = admission_guard.take() {
                guard.cancel(format!("blocked by gateway policy {}", block.policy_id));
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

        // `forward_body` was transformed and forced to include stream usage
        // before admission, so the exact bytes measured above are already the
        // bytes this request will forward.
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
    let (response, actual_usage) =
        enforce_output(&engine, &provider, &model, response, live_state.as_ref()).await;

    // --- SETTLEMENT (strict mode) ---
    //
    // `complete` with the real token counts when we recovered them; otherwise
    // the reservation rides the response body and `abandon`s when the body ends
    // or the client disconnects, leaving the conservative estimate applied.
    // Either way the send happens on a spawned task, never on the client's path.
    if let Some(guard) = admission_guard {
        // A stream's usage is not in the headers or a buffered body — it is in
        // the LAST SSE frame. Retaining the reservation estimate for it (the old
        // behavior) charges `input + max_tokens`, routinely ~100x a real
        // streamed reply. So tee the body: bytes pass through untouched while a
        // scanner reads the final usage frame, and settle from what it saw.
        if actual_usage.is_none() && is_event_stream(&response) {
            return attach_stream_settlement(response, guard, model.clone(), provider.clone());
        }
        return match actual_usage {
            Some(u) => {
                let cost = actual_usage_cost(&model, u);
                debug!(
                    reservation = %guard.reservation_id(), model = %model,
                    input_tokens = u.input_tokens, output_tokens = u.output_tokens, cost,
                    "Nova Guard: completing platform reservation with authoritative usage"
                );
                guard.complete(crate::policy::admission::SettlementUsage {
                    model: Some(model.clone()),
                    input_tokens: u64::from(u.input_tokens),
                    output_tokens: u64::from(u.output_tokens),
                    cost_usd: cost,
                    request_count: 1,
                    event_id: Some(crate::policy::usage::new_event_id()),
                    pricing_version: Some(crate::policy::pricing::CATALOG_VERSION.to_string()),
                });
                response
            }
            // Non-JSON bodies and bodies past the inspection cap land here: the
            // call reached the provider but no authoritative usage is
            // recoverable, so the conservative estimate has to stand.
            None => attach_body_guard(response, guard),
        };
    }

    // --- METERING (strict, but never reserved) ---
    //
    // The strict path ran yet we hold no reservation, so admission was
    // unavailable and the policy failed open. Nothing platform-side knows this
    // call happened: there is no reservation to settle, and the legacy telemetry
    // exporter is deliberately silent whenever admission owns metering (see
    // `NovaGuardUsagePlugin`). Report it here, or an outage of the admission API
    // would make every call it waved through invisible to the very cost cap that
    // waved it through.
    if strict_client.is_some() {
        report_unreserved_usage(gs.usage.as_ref(), &model, &response, actual_usage, estimate);
    } else if meter_allowed_here {
        // --- METERING (shared gateway, advisory path) ---
        //
        // Dedicated mode meters ALLOWED calls from the telemetry exporter, which
        // is registered once per process with one project's reporter. A shared
        // gateway has no such thing: the tenant is only known here. Without this
        // the platform counters would never advance for any tenant, and every
        // cost cap on a shared deployment would sit at $0 forever.
        //
        // A streamed response carries its usage in the LAST SSE frame, so it is
        // teed rather than metered from the estimate — otherwise every stream
        // would be reported at `input + max_tokens`, routinely ~100x reality.
        if actual_usage.is_none() && is_event_stream(&response) {
            let response = attach_stream_metering(
                response,
                gs.usage.clone(),
                model.clone(),
                provider.clone(),
                estimate,
                reservation.take(),
            );
            return response;
        }
        report_unreserved_usage(gs.usage.as_ref(), &model, &response, actual_usage, estimate);
    }

    // Transfer the guard we have held since `reserve()` into the response body,
    // so the reservation stays ACTIVE until the body finishes (or the client
    // disconnects): a long-lived or streaming request never loses its cap
    // protection mid-flight, and the post-completion TTL starts only once usage
    // reporting can actually begin. Note this is a *move*, not a new guard —
    // had the future been dropped before reaching here (client gone while we
    // awaited upstream headers), the same guard would already have completed
    // the reservation on the way out.
    match reservation {
        Some(guard) => attach_body_guard(response, guard),
        None => response,
    }
}

/// Wrap the response body so `guard` is dropped exactly when the body has been
/// fully streamed to the client — or the connection is dropped. The bytes pass
/// through untouched.
///
/// Generic over the guard type: a local
/// [`ReservationGuard`](crate::policy::remote::ReservationGuard) completes its
/// ledger entry on drop, a platform
/// [`AdmissionGuard`](crate::policy::admission::AdmissionGuard) settles its
/// reservation. Both must survive exactly as long as the body does.
fn attach_body_guard<G: Send + 'static>(response: Response, guard: G) -> Response {
    use futures_util::StreamExt;
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _keep_alive = &guard;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Is this an SSE response (i.e. one whose usage lives in its final frame)?
fn is_event_stream(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Wrap a streaming response so the provider's real token counts settle the
/// reservation, while every byte reaches the client unchanged and in order.
///
/// The body is **teed, not buffered**: each chunk is handed to the scanner and
/// then forwarded as the very same [`Bytes`](axum::body::Bytes) value, so chunk
/// boundaries, ordering and backpressure are exactly what they would be without
/// metering (the stream is still only polled by the downstream consumer, and
/// nothing is held back waiting for the usage frame).
///
/// Settlement happens when the wrapped stream is dropped — the body having been
/// fully read, or the client having disconnected — via [`StreamSettler`]'s
/// `Drop`, which is off the request path.
fn attach_stream_settlement(
    response: Response,
    guard: crate::policy::admission::AdmissionGuard,
    model: String,
    provider: String,
) -> Response {
    use futures_util::StreamExt;
    let (parts, body) = response.into_parts();
    let mut settler = StreamSettler::new(guard, model, provider);
    let stream = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = chunk.as_ref() {
            settler.observe(bytes);
        }
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Holds a platform reservation for the lifetime of a streaming body and settles
/// it from the usage the stream actually reported.
///
/// * usage recovered (OpenAI's `include_usage` chunk, Anthropic's
///   `message_delta`) → `complete`, reconciling the reservation DOWN from
///   `input + max_tokens` to what was really generated;
/// * no recoverable usage → `abandon`, which retains the conservative estimate.
///   That covers a client that disconnected mid-stream, a provider that
///   truncated before reporting, and a provider that never reports usage at all.
///
/// Exactly one settlement is ever sent: the guard is moved out of the `Option`
/// in `Drop`, and `AdmissionGuard`'s own `Drop` is what dispatches it.
struct StreamSettler {
    guard: Option<crate::policy::admission::AdmissionGuard>,
    scanner: crate::policy::metering::StreamUsageScanner,
    model: String,
    provider: String,
}

impl StreamSettler {
    fn new(
        guard: crate::policy::admission::AdmissionGuard,
        model: String,
        provider: String,
    ) -> Self {
        Self {
            guard: Some(guard),
            scanner: crate::policy::metering::StreamUsageScanner::new(),
            model,
            provider,
        }
    }

    /// Inspect one transport chunk. Never mutates or withholds it.
    fn observe(&mut self, bytes: &[u8]) {
        self.scanner.push(bytes);
    }
}

impl Drop for StreamSettler {
    fn drop(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        self.scanner.finish();
        match self.scanner.usage_priced(&self.model, &self.provider) {
            Some(u) => {
                let cost = actual_usage_cost(&self.model, u);
                debug!(
                    reservation = %guard.reservation_id(), model = %self.model,
                    input_tokens = u.input_tokens, output_tokens = u.output_tokens, cost,
                    "Nova Guard: completing platform reservation with a stream's reported usage"
                );
                guard.complete(crate::policy::admission::SettlementUsage {
                    model: Some(self.model.clone()),
                    input_tokens: u64::from(u.input_tokens),
                    output_tokens: u64::from(u.output_tokens),
                    cost_usd: cost,
                    request_count: 1,
                    event_id: Some(crate::policy::usage::new_event_id()),
                    pricing_version: Some(crate::policy::pricing::CATALOG_VERSION.to_string()),
                });
            }
            None => {
                debug!(
                    reservation = %guard.reservation_id(), model = %self.model,
                    "Nova Guard: stream ended without recoverable usage; retaining the estimate"
                );
                guard.abandon("stream ended without authoritative usage");
            }
        }
    }
}

/// Wrap a streaming response on the **shared gateway's advisory path** so the
/// ALLOWED usage event is reported from the provider's real token counts.
///
/// Same tee as [`attach_stream_settlement`] — bytes are forwarded untouched, in
/// order, with unchanged backpressure — but the terminal action is a usage
/// report rather than a reservation settlement. The local
/// [`ReservationGuard`](crate::policy::remote::ReservationGuard) rides along so
/// the pending ledger entry still completes exactly when the body does.
fn attach_stream_metering(
    response: Response,
    usage: Option<crate::policy::usage::UsageReporter>,
    model: String,
    provider: String,
    estimate: Option<RequestEstimate>,
    reservation: Option<crate::policy::remote::ReservationGuard>,
) -> Response {
    use futures_util::StreamExt;
    if !is_metered_response(&response) || usage.is_none() {
        return match reservation {
            Some(guard) => attach_body_guard(response, guard),
            None => response,
        };
    }
    let (parts, body) = response.into_parts();
    let mut meter = StreamMeter {
        usage,
        model,
        provider,
        estimate,
        _reservation: reservation,
        scanner: crate::policy::metering::StreamUsageScanner::new(),
    };
    let stream = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = chunk.as_ref() {
            meter.scanner.push(bytes);
        }
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Reports one ALLOWED event when a shared-gateway stream ends.
struct StreamMeter {
    usage: Option<crate::policy::usage::UsageReporter>,
    model: String,
    provider: String,
    estimate: Option<RequestEstimate>,
    /// Completes the pending-ledger entry on drop; never read.
    _reservation: Option<crate::policy::remote::ReservationGuard>,
    scanner: crate::policy::metering::StreamUsageScanner,
}

impl Drop for StreamMeter {
    fn drop(&mut self) {
        let Some(reporter) = self.usage.take() else {
            return;
        };
        if self.model.trim().is_empty() {
            return;
        }
        self.scanner.finish();
        // Measured counts when the stream reported them; otherwise the
        // conservative forward estimate — a call this layer could not measure
        // must not be metered at $0 (same choice `abandon` makes in strict mode).
        let (input_tokens, output_tokens, cost_usd) =
            match self.scanner.usage_priced(&self.model, &self.provider) {
                Some(u) => (
                    u.input_tokens,
                    u.output_tokens,
                    actual_usage_cost(&self.model, u),
                ),
                None => match self.estimate {
                    Some(e) => (e.input_tokens, e.output_tokens, e.cost_usd),
                    None => return,
                },
            };
        debug!(
            model = %self.model, input_tokens, output_tokens, cost_usd,
            "Nova Guard: metering a shared-gateway stream against its own tenant"
        );
        reporter.report(crate::policy::usage::UsageEvent::allowed(
            crate::policy::usage::new_event_id(),
            &self.model,
            cost_usd,
            input_tokens,
            output_tokens,
        ));
    }
}

/// This request's *forward* estimate, as charged at admission time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RequestEstimate {
    pub input_tokens: u32,
    /// The request's output limit, or the assumed completion size when it set none.
    pub output_tokens: u32,
    pub cost_usd: f64,
}

/// Header a Nova Guard synthetic block carries. A "successful" block is still a
/// block: in `SyntheticSuccess` mode it is served as a 200, so status alone
/// cannot tell a served call from a refused one.
const GUARD_BLOCKED_HEADER: &str = "x-noveum-guard-blocked";

/// Is this response metered usage? Applies the same filter as the telemetry
/// exporter — a synthetic block is not a model call, and neither is a failed
/// one — so every metering path in the gateway agrees on what counts.
fn is_metered_response(response: &Response) -> bool {
    !response.headers().contains_key(GUARD_BLOCKED_HEADER)
        && (200..300).contains(&response.status().as_u16())
}

/// Report one ALLOWED usage event for a call that ran **without** a platform
/// reservation while admission owned metering (strict mode, admission
/// unavailable, policy failed open).
///
/// Applies the same "is this metered usage?" filter as the telemetry exporter —
/// synthetic blocks and failed provider calls are not usage — so the two paths
/// can never disagree about which requests count.
fn report_unreserved_usage(
    usage: Option<&crate::policy::usage::UsageReporter>,
    model: &str,
    response: &Response,
    actual: Option<ActualUsage>,
    estimate: Option<RequestEstimate>,
) {
    let Some(reporter) = usage else { return };
    if model.trim().is_empty() || !is_metered_response(response) {
        return;
    }
    // The provider's own counts when the response carried them; otherwise the
    // conservative forward estimate — the same choice `abandon` makes for a
    // reservation, and for the same reason: a call this layer cannot measure
    // (a stream, a body past the inspection cap) must not be metered at $0.
    let (input_tokens, output_tokens, cost_usd) = match (actual, estimate) {
        (Some(u), _) => (u.input_tokens, u.output_tokens, actual_usage_cost(model, u)),
        (None, Some(e)) => (e.input_tokens, e.output_tokens, e.cost_usd),
        // No usage and no estimate: nothing honest to report.
        (None, None) => return,
    };
    debug!(
        model = %model, input_tokens, output_tokens, cost_usd, measured = actual.is_some(),
        "Nova Guard: metering a fail-open request that was never reserved"
    );
    reporter.report(crate::policy::usage::UsageEvent::allowed(
        crate::policy::usage::new_event_id(),
        model,
        cost_usd,
        input_tokens,
        output_tokens,
    ));
}

/// Prefer the provider-aware price recovered alongside authoritative usage;
/// retain token-only pricing for providers that report no detailed dimensions.
fn actual_usage_cost(model: &str, usage: ActualUsage) -> f64 {
    usage.cost_usd.unwrap_or_else(|| {
        crate::policy::pricing::price_usage(
            model,
            &crate::policy::pricing::BillableUsage::from_tokens(
                usage.input_tokens,
                usage.output_tokens,
            ),
        )
        .total_usd
    })
}

// Authoritative token counts + the parser that recovers them from a buffered
// provider body. Both live in `policy::metering` so the Worker bridge (which has
// no Tower/Axum layer) reads usage exactly the way this middleware does.
pub use crate::policy::metering::{extract_actual_usage, extract_actual_usage_priced, ActualUsage};

fn is_proxy_post<B>(req: &Request<B>) -> bool {
    if req.method() != axum::http::Method::POST {
        return false;
    }
    // Anchor to the proxy route prefix so unrelated JSON POSTs that merely
    // contain "/v1/" elsewhere in the path are not buffered/scanned.
    req.uri().path().starts_with("/v1/")
}

/// Should this request be inspected? POST, JSON, on the `/v1/` proxy path.
fn is_guardable<B>(req: &Request<B>) -> bool {
    if !is_proxy_post(req) {
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

/// Run output-phase enforcement, returning the (possibly rewritten or blocking)
/// response plus the provider's authoritative token counts when the body was
/// buffered and carried them. `None` usage means the caller must not claim to
/// know what this call consumed.
async fn enforce_output(
    engine: &PolicyEngine,
    provider: &str,
    model: &str,
    response: Response,
    live_state: Option<&crate::policy::rules::LiveState>,
) -> (Response, Option<ActualUsage>) {
    // Skip streaming responses (documented v1 limitation).
    let is_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);
    if is_stream {
        // A stream carries its usage in a final SSE frame, which this
        // buffer-the-body layer cannot see. It is NOT unrecoverable, though:
        // `attach_stream_settlement` tees the body and reads it as it flows, so
        // the reservation still settles on real numbers.
        debug!("Nova Guard: streaming response passed through without output enforcement (v1)");
        return (response, None);
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
        return (response, None);
    }

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            // Chunked response with no declared length exceeded the cap mid-read;
            // the original body is no longer recoverable. Return an honest error
            // envelope with a correct status rather than a 200 carrying a string.
            warn!("Nova Guard: chunked response exceeded inspection cap; returning 502");
            return (Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"error":{"message":"upstream response exceeded gateway inspection limit","type":"gateway_error"}}"#,
                ))
                .expect("static error response is valid"), None);
        }
    };

    let json: Option<Value> = serde_json::from_slice(&bytes).ok();
    let Some(body_json) = json else {
        // Not JSON (or empty) — pass through unchanged.
        return (Response::from_parts(parts, Body::from(bytes)), None);
    };

    // The provider's own token counts, if it reported them. Read BEFORE any
    // block/transform path so settlement is accurate even when output-phase
    // enforcement replaces the body: the model call happened either way.
    let actual_usage = extract_actual_usage_priced(model, provider, &body_json);

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
        return (Response::from_parts(parts, Body::from(bytes)), actual_usage);
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
        // The provider call already happened, so the reservation still settles
        // with the real usage even though the client gets a synthetic block.
        return (
            block_response(provider, model, block, engine.block_mode()),
            actual_usage,
        );
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
                return (Response::from_parts(parts, Body::from(v)), actual_usage);
            }
        }
    }

    (Response::from_parts(parts, Body::from(bytes)), actual_usage)
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

// ===========================================================================
// Shared-gateway tenancy layer (§6.4 / NOV-117)
// ===========================================================================
//
// In DEDICATED mode this layer does not exist: `build_router` only wires it
// when `NOVEUM_GUARD_TENANCY=shared`, and `guard_middleware` behaves exactly as
// it always has.
//
// In SHARED mode it runs OUTSIDE `guard_middleware` and owns tenant identity:
// it authenticates the caller against the platform, derives the project and
// organization from that credential, and hands `guard_middleware` a
// [`GuardState`] whose engine, live-state cache, pending ledger, usage reporter
// and admission client all belong to that one tenant. Nothing in the request —
// no header, no body field — can reach a tenant the credential is not entitled
// to, and no two tenants ever share a cache entry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::policy::remote::{
    credential_cache_key, select_tenant, CredentialCacheKey, GuardTenancy, RemoteConfig,
    SharedTenancyConfig, TenantId, TenantRejection, TenantResolver, ROUTING_ORG_HEADERS,
    ROUTING_PROJECT_HEADER, TENANT_CREDENTIAL_HEADER, TENANT_IDLE_TTL,
};

/// A single organization/project can legitimately rotate several API keys, but
/// retaining a poller, live-state cache and usage queue for every historical
/// key would make one hot tenant an unbounded resource sink.
const TENANT_CREDENTIAL_RUNTIME_MAX: usize = 64;

/// One caller credential's complete remote-client runtime for a derived tenant.
///
/// The policy/state data belongs to the tenant, but every HTTP client in this
/// object carries authentication. It therefore cannot be reused by a different
/// credential that happens to resolve to the same organization/project.
struct TenantRuntime {
    guard: GuardState,
    /// Per-tenant policy poller. Aborted when the tenant is evicted, so a
    /// shared gateway does not accumulate one polling task per tenant it has
    /// ever seen.
    _poller: AbortOnDrop,
}

struct CredentialRuntimeEntry {
    runtime: Arc<TenantRuntime>,
    last_used_ms: u64,
    access_order: u64,
}

/// Aborts its task on drop.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Per-tenant slot. Authorization-bearing runtimes are isolated per caller
/// credential, while the in-process pending ledger remains tenant-wide so two
/// keys for one project cannot each spend the same advisory headroom.
struct TenantSlot {
    runtimes: tokio::sync::Mutex<HashMap<CredentialCacheKey, CredentialRuntimeEntry>>,
    pending: Arc<crate::policy::remote::PendingSpend>,
    last_used: AtomicU64,
}

/// The shared-gateway tenancy registry.
pub struct SharedTenancy {
    cfg: SharedTenancyConfig,
    resolver: TenantResolver,
    engine_opts: crate::policy::engine::EngineOptions,
    cost_mode: Option<crate::policy::config::CostEnforcementMode>,
    allow_unguarded_start: bool,
    slots: std::sync::Mutex<HashMap<TenantId, Arc<TenantSlot>>>,
    started: Instant,
    runtime_access: AtomicU64,
}

impl SharedTenancy {
    pub fn new(
        cfg: SharedTenancyConfig,
        engine_opts: crate::policy::engine::EngineOptions,
        cost_mode: Option<crate::policy::config::CostEnforcementMode>,
        allow_unguarded_start: bool,
    ) -> Self {
        let resolver =
            TenantResolver::new(&cfg.base_url, cfg.resolution_ttl, cfg.max_tenants.max(64));
        Self {
            cfg,
            resolver,
            engine_opts,
            cost_mode,
            allow_unguarded_start,
            slots: std::sync::Mutex::new(HashMap::new()),
            started: Instant::now(),
            runtime_access: AtomicU64::new(0),
        }
    }

    /// Build from the environment for a validated [`GuardTenancy::Shared`].
    ///
    /// A present-but-unusable `NOVEUM_GUARD_COST_ENFORCEMENT` is an error here
    /// exactly as it is in dedicated mode: startup aborts rather than quietly
    /// downgrading strict caps to the per-process ledger.
    pub fn from_env(
        cfg: SharedTenancyConfig,
        allow_unguarded_start: bool,
    ) -> Result<Self, crate::policy::remote::RemoteConfigError> {
        let raw = std::env::var(crate::policy::admission::COST_ENFORCEMENT_VAR).ok();
        // `true`: the platform bridge IS configured in shared mode, just
        // per-caller rather than process-wide.
        let cost_mode = crate::policy::admission::enforcement_from_value(raw.as_deref(), true)?;
        let mut opts = crate::policy::engine::EngineOptions::from_env();
        // Every tenant's engine is backed by that tenant's `/state`, so
        // `failClosed` is honored rather than neutralized.
        opts.live_state_backed = true;
        Ok(Self::new(cfg, opts, cost_mode, allow_unguarded_start))
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Authenticate `credential`, derive its tenant, and return that tenant's
    /// isolated guard state. Every failure path is a refusal — there is no
    /// default tenant to fall through to.
    pub async fn guard_for(
        &self,
        credential: &str,
        project_hint: Option<&str>,
        org_hint: Option<&str>,
    ) -> Result<GuardState, TenantRejection> {
        let identity = self.resolver.resolve(credential).await?;
        // The authorization decision, made against the platform's answer.
        let tenant = select_tenant(&identity, project_hint, org_hint)?;
        let runtime = self.runtime_for(&tenant, credential).await?;
        Ok(runtime.guard.clone())
    }

    async fn runtime_for(
        &self,
        tenant: &TenantId,
        credential: &str,
    ) -> Result<Arc<TenantRuntime>, TenantRejection> {
        let slot = self.slot(tenant);
        let now = self.now_ms();
        let access_order = self.runtime_access.fetch_add(1, Ordering::Relaxed);
        slot.last_used.store(now, Ordering::Relaxed);
        let credential_key = credential_cache_key(credential);
        let mut runtimes = slot.runtimes.lock().await;
        let idle_ttl = u64::try_from(TENANT_IDLE_TTL.as_millis()).unwrap_or(u64::MAX);
        runtimes.retain(|_, entry| now.saturating_sub(entry.last_used_ms) < idle_ttl);
        if let Some(entry) = runtimes.get_mut(&credential_key) {
            entry.last_used_ms = now;
            entry.access_order = access_order;
            return Ok(entry.runtime.clone());
        }
        if runtimes.len() >= TENANT_CREDENTIAL_RUNTIME_MAX {
            let lru = runtimes
                .iter()
                .min_by(|(a_key, a), (b_key, b)| {
                    a.access_order
                        .cmp(&b.access_order)
                        .then_with(|| a_key.cmp(b_key))
                })
                .map(|(key, _)| *key);
            if let Some(lru) = lru {
                runtimes.remove(&lru);
                debug!(
                    tenant = %tenant,
                    "Nova Guard: evicting a rotated tenant credential runtime"
                );
            }
        }

        // Cold tenant: fetch ITS policy set with the caller's own credential.
        // Platform API keys are org-scoped, so this is also the authorization
        // check — a key cannot read another organization's policies even if the
        // gateway asked it to.
        let cfg = RemoteConfig::for_tenant(&self.cfg.base_url, credential, &tenant.project_id);
        let (engine, etag) = crate::policy::remote::bootstrap_engine(
            &cfg,
            self.engine_opts.clone(),
            self.allow_unguarded_start,
        )
        .await
        .map_err(|e| {
            // Same rule as dedicated startup: with no known policy set the
            // request would be forwarded unguarded, so it is refused instead.
            TenantRejection::Unavailable(format!("policy fetch for {tenant} failed: {e}"))
        })?;
        let engine = Arc::new(engine);
        let poller =
            crate::policy::remote::spawn_policy_poller_handle(cfg.clone(), engine.clone(), etag);
        let runtime = Arc::new(TenantRuntime {
            guard: GuardState {
                engine,
                live: Some(Arc::new(crate::policy::remote::RemoteLiveState::new(
                    cfg.clone(),
                ))),
                usage: Some(crate::policy::usage::UsageReporter::spawn(cfg.clone())),
                pending: slot.pending.clone(),
                admission: Some(Arc::new(crate::policy::admission::AdmissionClient::new(
                    cfg,
                    self.cost_mode,
                ))),
            },
            _poller: AbortOnDrop(poller),
        });
        info!(
            tenant = %tenant,
            credential = %crate::policy::remote::credential_fingerprint(credential),
            policies = runtime.guard.engine.active_policy_count(),
            "Nova Guard: warmed an isolated enforcement runtime for a tenant credential"
        );
        runtimes.insert(
            credential_key,
            CredentialRuntimeEntry {
                runtime: runtime.clone(),
                last_used_ms: now,
                access_order,
            },
        );
        drop(runtimes);
        self.evict();
        Ok(runtime)
    }

    fn slot(&self, tenant: &TenantId) -> Arc<TenantSlot> {
        let mut slots = self.slots.lock().expect("tenant registry lock poisoned");
        if let Some(slot) = slots.get(tenant) {
            return slot.clone();
        }
        let slot = Arc::new(TenantSlot {
            runtimes: tokio::sync::Mutex::new(HashMap::new()),
            pending: Arc::new(crate::policy::remote::PendingSpend::new()),
            last_used: AtomicU64::new(self.now_ms()),
        });
        slots.insert(tenant.clone(), slot.clone());
        slot
    }

    /// Drop idle tenants and, if still over the cap, the least recently used.
    /// Dropping a runtime aborts its poller and closes its usage queue (which
    /// makes a final best-effort flush), so nothing is silently stranded.
    fn evict(&self) {
        let now = self.now_ms();
        let idle_ttl = u64::try_from(TENANT_IDLE_TTL.as_millis()).unwrap_or(u64::MAX);
        // Collect the drop set under the lock, then release it before dropping
        // the runtimes (each drop touches a queue and a task handle).
        let doomed = {
            let mut slots = self.slots.lock().expect("tenant registry lock poisoned");
            let snapshot: Vec<(TenantId, u64)> = slots
                .iter()
                .map(|(t, s)| (t.clone(), s.last_used.load(Ordering::Relaxed)))
                .collect();
            let targets = crate::policy::remote::tenant_evictions(
                &snapshot,
                now,
                idle_ttl,
                self.cfg.max_tenants,
            );
            targets
                .into_iter()
                .filter_map(|t| {
                    slots.remove(&t).inspect(|_| {
                        debug!(tenant = %t, "Nova Guard: evicting an idle tenant runtime");
                    })
                })
                .collect::<Vec<_>>()
        };
        drop(doomed);
    }

    /// How many tenants are currently warm (diagnostics/tests).
    pub fn warm_tenants(&self) -> usize {
        self.slots
            .lock()
            .expect("tenant registry lock poisoned")
            .len()
    }
}

/// Does this path require an authenticated tenant? **Pure.**
///
/// Only the proxy surface. `/health` stays reachable so liveness/readiness
/// probes do not need a tenant credential.
pub fn requires_tenant(path: &str) -> bool {
    path.starts_with("/v1/")
}

/// The shared-gateway tenancy layer. Wired only in shared mode.
pub async fn tenant_middleware(
    State(shared): State<Arc<SharedTenancy>>,
    mut req: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    if !requires_tenant(req.uri().path()) {
        return next.run(req).await;
    }

    // Take the credential OUT of the request: it must never be forwarded to a
    // model provider, and nothing downstream has any business reading it.
    let raw_credential = req
        .headers_mut()
        .remove(TENANT_CREDENTIAL_HEADER)
        .and_then(|v| v.to_str().ok().map(str::to_string));
    let Some(credential) =
        crate::policy::remote::extract_credential(raw_credential.as_deref()).map(str::to_string)
    else {
        return tenant_rejection_response(&TenantRejection::MissingCredential);
    };

    let project_hint = header_str(&req, ROUTING_PROJECT_HEADER);
    let org_hint = ROUTING_ORG_HEADERS.iter().find_map(|h| header_str(&req, h));

    match shared
        .guard_for(&credential, project_hint.as_deref(), org_hint.as_deref())
        .await
    {
        Ok(guard) => {
            // Hand the derived tenant's isolated runtime to `guard_middleware`.
            req.extensions_mut().insert(guard);
            next.run(req).await
        }
        Err(rejection) => {
            warn!(
                credential = %crate::policy::remote::credential_fingerprint(&credential),
                code = rejection.code(), status = rejection.status(),
                requested_project = ?project_hint, requested_org = ?org_hint,
                "Nova Guard: refusing a request on the shared gateway"
            );
            tenant_rejection_response(&rejection)
        }
    }
}

/// Provider-shaped error envelope for a tenancy refusal.
pub fn tenant_rejection_response(rejection: &TenantRejection) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": rejection.message(),
            "type": rejection.error_type(),
            "code": rejection.code(),
        }
    });
    Response::builder()
        .status(
            axum::http::StatusCode::from_u16(rejection.status())
                .unwrap_or(axum::http::StatusCode::FORBIDDEN),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("tenancy rejection response is valid")
}

/// The guard wiring for one deployment mode, as decided at startup.
pub enum GuardWiring {
    /// One process-wide project (today's behavior, unchanged).
    Dedicated(GuardState),
    /// Per-caller tenancy; the process-wide `GuardState` is a no-op engine that
    /// is replaced per request by the derived tenant's own.
    Shared(Arc<SharedTenancy>),
}

impl GuardWiring {
    /// Which mode was configured, for logging.
    pub fn mode(&self) -> &'static str {
        match self {
            GuardWiring::Dedicated(_) => "dedicated",
            GuardWiring::Shared(_) => "shared",
        }
    }

    /// Name the mode a [`GuardTenancy`] selects, for startup logging.
    pub fn mode_of(tenancy: &GuardTenancy) -> &'static str {
        match tenancy {
            GuardTenancy::Dedicated(_) => "dedicated",
            GuardTenancy::Shared(_) => "shared",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_tenant_only_on_the_proxy_surface() {
        assert!(requires_tenant("/v1/chat/completions"));
        assert!(requires_tenant("/v1/messages"));
        // Probes must stay reachable without a tenant credential.
        assert!(!requires_tenant("/health"));
        assert!(!requires_tenant("/"));
        // Not anchored elsewhere in the path.
        assert!(!requires_tenant("/api/v1/chat"));
    }

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
    fn actual_usage_is_read_from_either_provider_spelling() {
        // OpenAI shape.
        assert_eq!(
            extract_actual_usage(&serde_json::json!({
                "usage": {"prompt_tokens": 123, "completion_tokens": 45, "total_tokens": 168}
            })),
            Some(ActualUsage {
                input_tokens: 123,
                output_tokens: 45,
                cost_usd: None,
            })
        );
        // Anthropic shape (reaches us on pass-through paths).
        assert_eq!(
            extract_actual_usage(&serde_json::json!({
                "usage": {"input_tokens": 7, "output_tokens": 9}
            })),
            Some(ActualUsage {
                input_tokens: 7,
                output_tokens: 9,
                cost_usd: None,
            })
        );
        // A half-reported usage block is not authoritative: settling it with
        // an invented zero would release spend the provider may have billed.
        assert_eq!(
            extract_actual_usage(&serde_json::json!({"usage": {"completion_tokens": 5}})),
            None
        );
    }

    #[test]
    fn absent_or_empty_usage_is_not_authoritative() {
        // Each of these must yield `None` so the caller ABANDONS the reservation
        // (estimate retained) instead of completing it with invented zeros —
        // completing at $0 would silently release the whole hold.
        for body in [
            serde_json::json!({"choices": [{"message": {"content": "hi"}}]}),
            serde_json::json!({"usage": {}}),
            serde_json::json!({"usage": {"total_tokens": 10}}),
            serde_json::json!({"usage": {"prompt_tokens": 7}}),
            serde_json::json!({"usage": null}),
            serde_json::json!({}),
        ] {
            assert_eq!(extract_actual_usage(&body), None, "body: {body}");
        }
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
