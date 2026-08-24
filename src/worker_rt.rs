//! Cloudflare Worker runtime (WASM).
//!
//! This is the `wasm32` entry point: a `#[event(fetch)]` handler that runs the
//! gateway as a true per‑PoP edge Worker. It reuses the **shared Nova Guard
//! engine** ([`crate::policy`]) and the shared request/response shaping
//! ([`crate::routing`]) for full parity with the native server, then proxies the
//! request to the upstream provider via the platform `Fetch` API (no
//! `reqwest`/`tokio`).
//!
//! Parity with the native server: same provider routing, same `x-provider`
//! contract, header pass‑through, query pass‑through, same Nova Guard
//! input/output decisions + redaction, the same block response modes
//! (`NOVEUM_GUARD_BLOCK_RESPONSE_MODE`), and the same Anthropic→OpenAI and
//! Bedrock-Converse→OpenAI response conversions. Bedrock is signed with AWS
//! SigV4 (see [`crate::sigv4`]) and additionally supports temporary credentials
//! (`x-aws-session-token`). Non‑JSON `/v1/*` bodies are forwarded byte‑for‑byte.
//!
//! # Nova Guard on this target
//!
//! Both shapes are supported:
//!
//! * **Inline, stateless** — a `NOVEUM_GUARD_POLICIES` bundle of text rules
//!   (`regex_match`, `pii_detection`, …), decided entirely from the payload in
//!   front of us. An absent bundle is a transparent proxy; a *malformed* one is
//!   a configuration error, never a silent pass-through.
//! * **Platform-managed** (`NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID`) — the
//!   effective policy set, live cost/rate counters and atomic admission come
//!   from the Noveum control plane over `worker::Fetch`
//!   ([`crate::policy::worker_remote`]). This replaces the previous blanket
//!   `503 gateway_configuration_error`.
//!
//! ## The platform request lifecycle
//!
//! 1. **Policies** — `GET /policies/effective`, cached per isolate and
//!    revalidated with `If-None-Match`. A failed *first* fetch refuses the
//!    request (503) unless `NOVEUM_GUARD_ALLOW_UNGUARDED_START` is set; a failed
//!    *refresh* keeps the last known set.
//! 2. **Admission** — when the set contains `cost_cap`/`rate_limit`, `POST
//!    /policies/admit` reserves this request's estimate against one counter
//!    shared by every PoP. A block is HTTP 200 with `allowed:false`; a 503 is
//!    *unavailable* and is resolved by the policy's `failClosed`, never by
//!    admitting the call.
//! 3. **Enforcement** — the shared engine runs the input phase against the live
//!    counters, exactly as it does natively. A gateway-side block **cancels**
//!    the reservation: that call provably never reached the provider.
//! 4. **Settlement** — every admitted request settles exactly once, scheduled on
//!    [`Context::wait_until`] *before* the response is returned. Buffered
//!    responses settle from their `usage` object; streams settle from the
//!    terminal SSE usage frame, recovered by teeing the body through
//!    [`StreamUsageScanner`] without buffering a byte.
//!
//! Inline `cost_cap`/`rate_limit` policies with **no** platform bridge are still
//! refused: without a live-state backend they could only ever evaluate to
//! "allow", and their `failClosed` setting would be neutralized along with them.

use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use futures::channel::oneshot;
use futures_util::future::{select, Either};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use worker::*;

use crate::policy::decision::{Phase, PolicyDecision};
use crate::policy::engine::EngineOptions;
use crate::policy::metering::{extract_actual_usage_priced, ActualUsage, StreamUsageScanner};
use crate::policy::rules::LiveState;
use crate::policy::synthetic::{block_body, block_status, policy_header_token, BlockResponseMode};
use crate::policy::worker_remote::{
    self, admit_request_body, assumed_output_tokens_from_value, force_include_usage,
    resolve_max_output_tokens, Admission, AdmitRequest, BodyAdmission, Settlement, StreamOutcome,
    WorkerRemoteConfig, ALLOW_UNGUARDED_START_VAR, API_KEY_VAR, API_URL_VAR,
    ASSUMED_OUTPUT_TOKENS_VAR, MAX_OUTPUT_TOKEN_LIMIT, PROJECT_ID_VAR, TENANCY_VAR,
};
use crate::policy::PolicyEngine;
use crate::routing::{
    apply_input_transforms, apply_output_transforms, authorization_bearer_token,
    bedrock_converse_to_openai, estimate_admission_input_tokens, flatten_input_text,
    flatten_output_text, normalize_base_url,
    openai_to_anthropic_messages_with_assumed_output_tokens, openai_to_bedrock_converse,
    prepare_strict_admission_body, resolve_provider, transform_anthropic_to_openai_format,
    upstream_url_with_base, validate_strict_anthropic_base_url, validate_strict_openai_base_url,
    ANTHROPIC_BASE_URL_VAR, OPENAI_BASE_URL_VAR,
};
use crate::sigv4;

/// Default Bedrock model + region (mirrors the native `BedrockProvider`).
const BEDROCK_DEFAULT_MODEL: &str = "amazon.titan-text-premier-v1:0";
const BEDROCK_DEFAULT_REGION: &str = "us-east-1";
/// The platform reaps a pending reservation after 15 minutes. End an edge
/// provider call well before that lease can be released under a still-live SSE
/// stream; Cloudflare otherwise permits incoming Worker requests indefinitely.
const MAX_RESERVED_UPSTREAM_MS: u64 = 10 * 60 * 1_000;
const UPSTREAM_TIMEOUT_VAR: &str = "NOVEUM_GUARD_WORKER_UPSTREAM_TIMEOUT_MS";

/// AWS credentials for a Bedrock request, taken from `x-aws-*` headers.
struct BedrockCreds {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    region: String,
}

/// Extract Bedrock credentials from request headers (supports temporary creds
/// via `x-aws-session-token`, which the native path does not).
fn bedrock_credentials(req: &Request) -> Option<BedrockCreds> {
    let h = req.headers();
    let access_key = h
        .get("x-aws-access-key-id")
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let secret_key = h
        .get("x-aws-secret-access-key")
        .ok()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let session_token = h.get("x-aws-session-token").ok().flatten();
    let region = h
        .get("x-aws-region")
        .ok()
        .flatten()
        .unwrap_or_else(|| BEDROCK_DEFAULT_REGION.to_string());
    Some(BedrockCreds {
        access_key,
        secret_key,
        session_token,
        region,
    })
}

/// Current UTC time as SigV4 `(amz_date = YYYYMMDDTHHMMSSZ, datestamp = YYYYMMDD)`.
fn amz_timestamps() -> (String, String) {
    let d = js_sys::Date::new_0();
    let year = d.get_utc_full_year();
    let month = d.get_utc_month() + 1; // JS months are 0-based
    let day = d.get_utc_date();
    let hour = d.get_utc_hours();
    let min = d.get_utc_minutes();
    let sec = d.get_utc_seconds();
    let datestamp = format!("{year:04}{month:02}{day:02}");
    let amz_date = format!("{datestamp}T{hour:02}{min:02}{sec:02}Z");
    (amz_date, datestamp)
}

/// A fresh idempotency key. Matches `policy::usage::new_event_id` (native-only,
/// because that module is `reqwest`-bound).
fn new_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The bytes to forward upstream: rewritten JSON re-serialized, else the
/// original request bytes verbatim.
fn forward_body_bytes(rewritten: bool, body_json: &Option<Value>, original: &[u8]) -> Vec<u8> {
    if rewritten {
        if let Some(j) = body_json {
            return serde_json::to_vec(j).unwrap_or_else(|_| original.to_vec());
        }
    }
    original.to_vec()
}

/// Max body size we will buffer, for the request and for output-phase
/// inspection alike. A request past it is rejected with `413`; a response past
/// it passes through uninspected (matches native; avoids OOM on the 128 MB
/// isolate).
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Request headers we never forward upstream: hop-by-hop, length/encoding (the
/// runtime recomputes them and we send a decoded body), and our routing header.
/// `accept-encoding` is dropped so the upstream returns an inspectable
/// (uncompressed) body, matching native reqwest's auto-decode behavior.
const REQUEST_SKIP_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-encoding",
    "accept-encoding",
    "connection",
    "transfer-encoding",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
    "x-provider",
    "x-noveum-api-key",
    "x-noveum-guard-policy",
    "x-noveum-guard-blocked",
    "x-project-id",
    "x-organization-id",
    "x-organisation-id",
    "x-user-id",
    "x-experiment-id",
    "cookie",
];

/// Response headers we drop when re-emitting a buffered/transformed body (its
/// length and encoding change); everything else (x-request-id, rate-limit, …)
/// is preserved.
const RESPONSE_SKIP_HEADERS: &[&str] = &["content-length", "content-encoding"];

/// Read a Worker variable, preferring a `wrangler secret` over a plain `[vars]`
/// entry. `Some("")` is deliberately distinct from `None`: an operator who set
/// `NOVEUM_API_KEY=""` is *trying* to enable the bridge and gets an error, not a
/// silent unguarded proxy.
fn env_value(env: &Env, key: &str) -> Option<String> {
    env.secret(key)
        .map(|v| v.to_string())
        .or_else(|_| env.var(key).map(|v| v.to_string()))
        .ok()
}

fn env_flag(env: &Env, key: &str, default: bool) -> bool {
    match env_value(env, key) {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off" | "disabled" | ""
        ),
        None => default,
    }
}

fn worker_assumed_output_tokens(env: &Env) -> u64 {
    assumed_output_tokens_from_value(env_value(env, ASSUMED_OUTPUT_TOKENS_VAR).as_deref())
}

fn reserved_upstream_deadline_ms(env: &Env) -> u64 {
    let configured = env_value(env, UPSTREAM_TIMEOUT_VAR)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_RESERVED_UPSTREAM_MS)
        .min(MAX_RESERVED_UPSTREAM_MS);
    Date::now().as_millis().saturating_add(configured)
}

fn remaining_until(deadline_ms: u64) -> Option<Duration> {
    let remaining = deadline_ms.saturating_sub(Date::now().as_millis());
    (remaining > 0).then(|| Duration::from_millis(remaining))
}

async fn before_upstream_deadline<T>(
    deadline_ms: u64,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    let remaining = remaining_until(deadline_ms)?;
    futures_util::pin_mut!(future);
    match select(future, Delay::from(remaining)).await {
        Either::Left((value, _)) => Some(value),
        Either::Right((_, _)) => None,
    }
}

/// Engine options from the Worker environment, shared by the inline and the
/// platform-managed paths so block shapes and the master switch behave the same
/// either way.
///
/// `live_state_backed` mirrors the native bootstrap (`main.rs`): it is `true`
/// only when the platform bridge is configured. With no state backend the engine
/// neutralizes `failClosed` on `cost_cap`/`rate_limit` at compile time — no
/// state can ever arrive, so honoring it would block all traffic permanently.
fn engine_options(env: &Env, live_state_backed: bool) -> EngineOptions {
    EngineOptions {
        enabled: env_flag(env, "NOVEUM_GUARD_ENABLED", true),
        block_mode: env_value(env, "NOVEUM_GUARD_BLOCK_RESPONSE_MODE")
            .map(|v| BlockResponseMode::from_env_str(&v))
            .unwrap_or(BlockResponseMode::SyntheticSuccess),
        live_state_backed,
        ..Default::default()
    }
}

/// Build the Nova Guard engine from the in‑memory `NOVEUM_GUARD_POLICIES`
/// bundle.
///
/// On the edge there is no filesystem. An *absent* bundle is a transparent
/// pass‑through, exactly like the native `from_env`; a *present but malformed*
/// one is a configuration error (`Err`) that the caller turns into a 503 —
/// parsing it away would leave the operator believing the policies they deployed
/// are in force.
fn build_inline_engine(
    env: &Env,
    opts: EngineOptions,
) -> core::result::Result<PolicyEngine, String> {
    use crate::policy::config::PolicyBundle;

    let configured = env_value(env, "NOVEUM_GUARD_POLICIES").filter(|s| !s.trim().is_empty());
    let bundle = match configured {
        None => PolicyBundle::default(),
        Some(s) => PolicyBundle::from_json_str(&s).map_err(|e| {
            format!("NOVEUM_GUARD_POLICIES is set but is not a valid nova-guard bundle: {e}")
        })?,
    };
    Ok(PolicyEngine::from_bundle(&bundle, opts))
}

/// Copy `src` headers into a fresh `Headers`, skipping any whose (lowercased)
/// name is in `skip`.
fn copy_headers_excluding(src: &Headers, skip: &[&str]) -> Result<Headers> {
    let out = Headers::new();
    let entries = src.entries().collect::<Vec<_>>();
    let connection_tokens = entries
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    for (k, v) in entries {
        let lower = k.to_ascii_lowercase();
        if skip.contains(&lower.as_str())
            || lower.starts_with("x-noveum-")
            || lower.starts_with("x-aws-")
            || connection_tokens.iter().any(|token| token == &lower)
        {
            continue;
        }
        out.set(&k, &v)?;
    }
    Ok(out)
}

/// Build a Nova Guard block response in the engine's configured mode, using the
/// SHARED body/status builders so it is byte-identical to the native server.
fn guard_block_response(
    provider: &str,
    model: &str,
    decision: &PolicyDecision,
    mode: BlockResponseMode,
) -> Result<Response> {
    let body = block_body(provider, model, decision, mode);
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    headers.set("x-noveum-guard-blocked", "true")?;
    headers.set(
        "x-noveum-guard-policy",
        &policy_header_token(&decision.policy_id),
    )?;
    Ok(Response::from_json(&body)?
        .with_headers(headers)
        .with_status(block_status(mode)))
}

/// A JSON error envelope in the providers' shape.
fn error_response(status: u16, kind: &str, message: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": { "message": message, "type": kind }
    }))?
    .with_headers(headers)
    .with_status(status))
}

/// 503 for a Nova Guard configuration this deployment target cannot enforce.
/// Failing loudly is the point: a silent no-op would leave the operator
/// believing a cap or a fail-closed policy is in force.
fn unsupported_guard_config(message: &str) -> Result<Response> {
    error_response(503, "gateway_configuration_error", message)
}

/// Provider-shaped 400 for an unusable output limit. Mirrors the native
/// `middleware::invalid_output_limit_response`.
fn invalid_output_limit(key: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": format!(
                "`{key}` must be a positive integer no greater than {MAX_OUTPUT_TOKEN_LIMIT}"
            ),
            "type": "invalid_request_error",
            "param": key,
            "code": "invalid_value",
        }
    }))?
    .with_headers(headers)
    .with_status(400))
}

/// A strict cost-cap reservation must cover the provider's whole possible
/// completion. The configured 1,024-token fallback is useful for advisory
/// estimates, but cannot make an otherwise unbounded request a hard cap.
fn missing_strict_output_limit() -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": "a strict Nova Guard cost cap requires an explicit output limit (`max_tokens`, `max_completion_tokens`, or `max_output_tokens`)",
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "missing_output_limit",
        }
    }))?
    .with_headers(headers)
    .with_status(400))
}

/// Provider-shaped 400 for a request whose provider-side input cannot be
/// bounded before a strict cost-cap admission. Mirrors native middleware.
fn invalid_strict_input(message: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "unsupported_strict_input",
        }
    }))?
    .with_headers(headers)
    .with_status(400))
}

/// Provider-shaped 400 for a request an active cost/rate policy cannot admit
/// and meter. Forwarding it would silently bypass the authoritative counters.
fn invalid_stateful_input(message: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "unsupported_stateful_input",
        }
    }))?
    .with_headers(headers)
    .with_status(400))
}

fn transformed_body_too_large() -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": format!(
                "transformed request body exceeds the {MAX_BODY}-byte gateway limit"
            ),
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": "request_too_large",
        }
    }))?
    .with_headers(headers)
    .with_status(413))
}

/// Read a response body with a hard byte cap. Reads incrementally so a chunked /
/// missing-`Content-Length` body cannot blow past the cap and OOM the isolate.
/// `Err(())` means the body exceeded the cap (it can't be safely re-streamed) —
/// the caller returns 502, matching native's behavior.
async fn read_body_capped(resp: &mut Response) -> core::result::Result<Vec<u8>, ()> {
    let mut stream = resp.stream().map_err(|_| ())?;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Read the *request* body under the same hard cap.
///
/// `req.bytes()` has no bound at all: a client that streams a chunked body with
/// no `Content-Length` (or lies about it) could allocate until the isolate is
/// killed. The declared length is checked first by
/// [`admit_request_body`], and this enforces the same bound while reading so a
/// chunked body cannot slip past it. `Err(())` → the caller returns `413`.
async fn read_request_body_capped(req: &mut Request) -> core::result::Result<Vec<u8>, ()> {
    let mut stream = match req.stream() {
        Ok(s) => s,
        // No body at all (or an unreadable one): treat as empty, matching the
        // previous `req.bytes().unwrap_or_default()`.
        Err(_) => return Ok(Vec::new()),
    };
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Apply permissive CORS headers, matching the native `CorsLayer` (any origin /
/// method / header). Works on responses we construct (mutable headers) and on
/// pass-through responses re-wrapped by [`passthrough`].
fn apply_cors(resp: Response) -> Response {
    let h = resp.headers();
    let _ = h.set("access-control-allow-origin", "*");
    let _ = h.set("access-control-allow-methods", "*");
    let _ = h.set("access-control-allow-headers", "*");
    let _ = h.set("access-control-max-age", "3600");
    resp
}

/// Re-wrap an upstream `Fetch` response so its headers become mutable (the raw
/// Fetch response has an immutable header set). The body is re-streamed lazily —
/// no buffering — so SSE/streaming pass-through is preserved. This lets the
/// outer CORS layer attach headers even on the transparent proxy path.
fn passthrough(mut resp: Response) -> Result<Response> {
    let status = resp.status_code();
    // Drop length/encoding: `from_stream` re-frames the body (chunked), so a
    // copied Content-Length would be stale, and the bytes are already decoded.
    let headers = copy_headers_excluding(resp.headers(), RESPONSE_SKIP_HEADERS)?;
    let stream = resp.stream()?;
    Ok(Response::from_stream(stream)?
        .with_headers(headers)
        .with_status(status))
}

// ---------------------------------------------------------------------------
// Platform reservation lifecycle
// ---------------------------------------------------------------------------

/// A platform reservation that has been granted and not yet settled.
///
/// Held by value so the type system tracks it: settling *consumes* it, and the
/// one place it can be dropped without an explicit outcome ([`handle`]'s
/// backstop) abandons it, which is the conservative choice — the call may well
/// have reached the provider.
struct Pending {
    cfg: WorkerRemoteConfig,
    id: String,
    model: String,
    provider: String,
    upstream_deadline_ms: u64,
}

impl Pending {
    /// Schedule the settlement on the fetch event and consume the reservation.
    ///
    /// `wait_until` is the *only* way settlement is ever started: nothing here
    /// uses `spawn_local`, so there is no promise the runtime does not know
    /// about, and the isolate cannot be torn down with the call in flight.
    fn settle(self, ctx: &Context, settlement: Settlement) {
        let Pending { cfg, id, .. } = self;
        ctx.wait_until(async move {
            worker_remote::settle(&cfg, &id, &settlement).await;
        });
    }

    /// Settle from a buffered response body: authoritative `usage` completes,
    /// anything else abandons (the conservative estimate stays applied). The
    /// decision itself lives in [`worker_remote::buffered_body_outcome`], where
    /// it is unit-tested natively.
    fn settle_from_body(self, ctx: &Context, body: Option<&Value>, raw_usage: Option<ActualUsage>) {
        let outcome = worker_remote::buffered_body_outcome(body, raw_usage);
        let settlement = worker_remote::settlement_for(outcome, &self.model, Some(new_event_id()));
        self.settle(ctx, settlement);
    }
}

/// Tee a provider stream: every byte is forwarded **unchanged and in order**
/// while a [`StreamUsageScanner`] reads the terminal usage frame out of the same
/// bytes.
///
/// Nothing is buffered and nothing is held back — a chunk is scanned and yielded
/// in the same step, so the consumer's demand still drives the producer and
/// backpressure is preserved end to end. At EOF the outcome is published on
/// `tx`; if the body is dropped first (client disconnect, isolate teardown) the
/// sender drops with it and the receiver resolves to [`StreamOutcome::Dropped`],
/// so the settlement future can never wait forever.
fn tee_usage<S>(
    inner: S,
    tx: oneshot::Sender<StreamOutcome>,
    model: String,
    provider: String,
) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin + 'static,
{
    struct Tee<S> {
        inner: S,
        scanner: StreamUsageScanner,
        tx: Option<oneshot::Sender<StreamOutcome>>,
        model: String,
        provider: String,
    }

    futures_util::stream::unfold(
        Tee {
            inner,
            scanner: StreamUsageScanner::new(),
            tx: Some(tx),
            model,
            provider,
        },
        |mut tee: Tee<S>| async move {
            match tee.inner.next().await {
                Some(Ok(chunk)) => {
                    tee.scanner.push(&chunk);
                    Some((Ok(chunk), tee))
                }
                // A transport error mid-stream: the client sees the failure and
                // no usage is authoritative. The sender rides along and drops
                // with the consumer, settling as `Dropped` → abandon.
                Some(Err(e)) => Some((Err(e), tee)),
                None => {
                    tee.scanner.finish();
                    if let Some(tx) = tee.tx.take() {
                        let _ =
                            tx.send(match tee.scanner.usage_priced(&tee.model, &tee.provider) {
                                Some(u) => StreamOutcome::Usage(u),
                                None => StreamOutcome::EndedWithoutUsage,
                            });
                    }
                    None
                }
            }
        },
    )
}

/// Stream a provider response straight through while metering it.
///
/// The settlement future is handed to `wait_until` **before** this returns, so
/// the response never leaves the handler with an unowned promise behind it.
type WorkerByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>>>>>;

/// End an admitted provider body before the platform's 15-minute reservation
/// lease can be reaped. The emitted transport error closes the client stream;
/// `tee_usage` then abandons the reservation, retaining its conservative hold.
fn deadline_stream<S>(inner: S, deadline_ms: u64) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin + 'static,
{
    struct State<S> {
        inner: S,
        deadline_ms: u64,
        timed_out: bool,
    }

    futures_util::stream::unfold(
        State {
            inner,
            deadline_ms,
            timed_out: false,
        },
        |mut state| async move {
            if state.timed_out {
                return None;
            }
            let Some(remaining) = remaining_until(state.deadline_ms) else {
                state.timed_out = true;
                return Some((
                    Err(Error::RustError(
                        "upstream stream exceeded the NovaGuard reservation lease budget"
                            .to_string(),
                    )),
                    state,
                ));
            };
            let next = state.inner.next();
            futures_util::pin_mut!(next);
            match select(next, Delay::from(remaining)).await {
                Either::Left((item, _)) => item.map(|item| (item, state)),
                Either::Right((_, _)) => {
                    state.timed_out = true;
                    Some((
                        Err(Error::RustError(
                            "upstream stream exceeded the NovaGuard reservation lease budget"
                                .to_string(),
                        )),
                        state,
                    ))
                }
            }
        },
    )
}

/// Drive the same pure Anthropic SSE state machine as the native Axum adapter
/// over a Worker `Fetch` body. Error frames are flushed before a transport
/// error, and `[DONE]` is emitted only for a complete `message_stop` stream.
fn translate_anthropic_stream<S>(inner: S, created: i64) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin + 'static,
{
    enum Translate<S> {
        Body(S, crate::anthropic_stream::AnthropicStreamTransformer),
        Abort(String),
        Done,
    }

    futures_util::stream::unfold(
        Translate::Body(
            inner,
            crate::anthropic_stream::AnthropicStreamTransformer::new(created),
        ),
        |state| async move {
            match state {
                Translate::Body(mut upstream, mut transformer) => loop {
                    match upstream.next().await {
                        Some(Ok(bytes)) => {
                            let out = transformer.push(&bytes);
                            if let Some(error) = out.fatal {
                                return Some((Ok(out.sse.into_bytes()), Translate::Abort(error)));
                            }
                            if out.sse.is_empty() {
                                continue;
                            }
                            return Some((
                                Ok(out.sse.into_bytes()),
                                Translate::Body(upstream, transformer),
                            ));
                        }
                        Some(Err(error)) => {
                            let out = transformer.fail_transport(format!("{error:?}"));
                            return Some((
                                Ok(out.sse.into_bytes()),
                                Translate::Abort(out.fatal.unwrap_or_default()),
                            ));
                        }
                        None => {
                            let out = transformer.finish();
                            return match out.fatal {
                                Some(error) => {
                                    Some((Ok(out.sse.into_bytes()), Translate::Abort(error)))
                                }
                                None if out.sse.is_empty() => None,
                                None => Some((Ok(out.sse.into_bytes()), Translate::Done)),
                            };
                        }
                    }
                },
                Translate::Abort(error) => Some((Err(Error::RustError(error)), Translate::Done)),
                Translate::Done => None,
            }
        },
    )
}

/// Re-stream an upstream event stream, optionally translating Anthropic's
/// protocol before the existing usage tee. Translating first is essential: the
/// scanner and the OpenAI client both consume the terminal OpenAI usage chunk.
fn stream_passthrough(
    mut resp: Response,
    pending: Option<Pending>,
    ctx: &Context,
    translate_anthropic: bool,
) -> Result<Response> {
    let status = resp.status_code();
    let headers = copy_headers_excluding(resp.headers(), RESPONSE_SKIP_HEADERS)?;
    let upstream: WorkerByteStream = match pending.as_ref() {
        Some(hold) => Box::pin(deadline_stream(resp.stream()?, hold.upstream_deadline_ms)),
        None => Box::pin(resp.stream()?),
    };
    let stream: WorkerByteStream = if translate_anthropic {
        let created = (Date::now().as_millis() / 1000) as i64;
        Box::pin(translate_anthropic_stream(upstream, created))
    } else {
        Box::pin(upstream)
    };

    if let Some(pending) = pending {
        let (tx, rx) = oneshot::channel::<StreamOutcome>();
        let Pending {
            cfg,
            id,
            model,
            provider,
            ..
        } = pending;
        let settlement_model = model.clone();
        ctx.wait_until(async move {
            // `Err` = the sender was dropped with the body, i.e. the client
            // vanished before the final frame.
            let outcome = rx.await.unwrap_or(StreamOutcome::Dropped);
            let settlement =
                worker_remote::settlement_for(outcome, &settlement_model, Some(new_event_id()));
            worker_remote::settle(&cfg, &id, &settlement).await;
        });

        return Ok(
            Response::from_stream(tee_usage(stream, tx, model, provider))?
                .with_headers(headers)
                .with_status(status),
        );
    }

    Ok(Response::from_stream(stream)?
        .with_headers(headers)
        .with_status(status))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    // CORS preflight — mirror the native permissive CorsLayer.
    if req.method() == Method::Options {
        return Ok(apply_cors(Response::empty()?.with_status(204)));
    }
    let resp = handle(req, env, ctx).await?;
    Ok(apply_cors(resp))
}

async fn handle(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let mut pending: Option<Pending> = None;
    let out = proxy(req, env, &ctx, &mut pending).await;
    // Backstop. `proxy` settles explicitly on every outcome it recognizes; this
    // catches the ones it cannot — an upstream `Fetch` failure propagated with
    // `?`, a header-construction error, a future edit that adds an early return.
    // Abandoning (rather than cancelling) is the conservative choice: the call
    // may have reached the provider, so the estimate must stay applied. A
    // reservation therefore cannot escape this function unsettled.
    if let Some(p) = pending.take() {
        console_warn!(
            "Nova Guard: request ended without an explicit settlement; abandoning reservation {}",
            p.id
        );
        p.settle(
            &ctx,
            Settlement::Abandon("request ended before settlement".to_string()),
        );
    }
    out
}

async fn proxy(
    mut req: Request,
    env: Env,
    ctx: &Context,
    pending: &mut Option<Pending>,
) -> Result<Response> {
    let path = req.path();

    if path == "/health" {
        return Response::from_json(&json!({
            "status": "healthy",
            "version": env!("CARGO_PKG_VERSION"),
            "runtime": "cloudflare-worker",
        }));
    }

    if req.method() != Method::Post || !path.starts_with("/v1/") {
        return Response::error("Not Found", 404);
    }

    // --- Deployment mode -----------------------------------------------------
    //
    // Shared tenancy is native-only. Reading the variable and refusing is the
    // point: unread, `NOVEUM_GUARD_TENANCY=shared` would proxy every caller
    // unguarded on a deployment its operator believes enforces per-tenant caps.
    if let Some(message) = worker_remote::tenancy_refusal(env_value(&env, TENANCY_VAR).as_deref()) {
        console_error!("Nova Guard configuration error: {message}");
        return unsupported_guard_config(&message);
    }

    // --- Platform bridge configuration -------------------------------------
    //
    // Same matrix as the native `RemoteConfig::from_values`: both vars → the
    // bridge is on; neither → off; exactly one, or either blank → a
    // configuration error, because that operator is *trying* to enable
    // enforcement and must not be rewarded with an unguarded 200.
    let bridge = match WorkerRemoteConfig::from_values(
        env_value(&env, API_KEY_VAR).as_deref(),
        env_value(&env, PROJECT_ID_VAR).as_deref(),
        env_value(&env, API_URL_VAR).as_deref(),
    ) {
        Ok(v) => v,
        Err(message) => {
            console_error!("Nova Guard configuration error: {message}");
            return unsupported_guard_config(&message);
        }
    };
    let assumed_output_tokens = worker_assumed_output_tokens(&env);

    let provider = req
        .headers()
        .get("x-provider")
        .ok()
        .flatten()
        .unwrap_or_else(|| "openai".to_string());
    let is_anthropic = provider.eq_ignore_ascii_case("anthropic");
    let is_bedrock = provider.eq_ignore_ascii_case("bedrock");
    let route = resolve_provider(&provider);
    if route.is_none() && !is_anthropic && !is_bedrock {
        return Response::error(
            format!("Unsupported or not-yet-ported provider on edge: {provider}"),
            400,
        );
    }

    // Preserve the query string (native forwards it).
    let query = req
        .url()
        .ok()
        .and_then(|u| u.query().map(|q| q.to_string()));

    // Only JSON bodies are parsed/guarded/transformed. Non-JSON (multipart,
    // binary) bodies are forwarded byte-for-byte, like the native proxy.
    let is_json_req = req
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .map(|ct| ct.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);

    // --- Policy set ---------------------------------------------------------
    let opts = engine_options(&env, bridge.is_some());
    let engine: Rc<PolicyEngine> = match &bridge {
        Some(cfg) => {
            // The platform is the source of truth once the bridge is configured:
            // an operator who pointed the Worker at a project did not ask for
            // whatever happens to be inlined in `[vars]`.
            if env_value(&env, "NOVEUM_GUARD_POLICIES").is_some_and(|s| !s.trim().is_empty()) {
                console_warn!(
                    "Nova Guard: both the platform bridge and NOVEUM_GUARD_POLICIES are set; the \
                     platform's effective policy set wins and the inline bundle is ignored"
                );
            }
            match worker_remote::effective_engine(cfg, opts.clone()).await {
                Ok(engine) => engine,
                Err(e) if env_flag(&env, ALLOW_UNGUARDED_START_VAR, false) => {
                    // ERROR, not WARN: the Worker is serving and enforcing
                    // nothing. This line is the only signal that a cap the
                    // operator believes is live is not.
                    console_error!(
                        "Nova Guard: platform policy fetch FAILED ({e}) and \
                         {ALLOW_UNGUARDED_START_VAR} is set; serving this request with NO \
                         enforcement"
                    );
                    Rc::new(PolicyEngine::from_bundle(
                        &crate::policy::config::PolicyBundle::default(),
                        opts,
                    ))
                }
                Err(e) => {
                    console_error!("Nova Guard: platform policy fetch failed: {e}");
                    return error_response(
                        503,
                        "gateway_configuration_error",
                        &format!(
                            "platform-managed Nova Guard is configured but its policy set could \
                             not be fetched ({e}), so no policy set is known and this request \
                             would be forwarded unguarded. Fix connectivity/credentials, or set \
                             {ALLOW_UNGUARDED_START_VAR}=true to serve unguarded anyway \
                             (emergency use only — caps and fail-closed policies will NOT be \
                             enforced)."
                        ),
                    );
                }
            }
        }
        None => match build_inline_engine(&env, opts) {
            Ok(engine) => Rc::new(engine),
            // A malformed inline bundle is a configuration error, not a
            // pass-through: proxying would silently drop every policy written.
            Err(e) => {
                console_error!("Nova Guard configuration error: {e}");
                return unsupported_guard_config(&e);
            }
        },
    };

    let guard_active = engine.is_enabled() && engine.active_policy_count() > 0;
    let block_mode = engine.block_mode();
    // `cost_cap`/`rate_limit` need a live cross-request state backend. The
    // platform bridge IS that backend; without it they would evaluate to "allow"
    // on every request and `failClosed` would be neutralized along with them.
    let stateful = engine.is_enabled() && engine.stateful_policy_count() > 0;
    if stateful && bridge.is_none() {
        return unsupported_guard_config(
            "NOVEUM_GUARD_POLICIES contains cost_cap/rate_limit policies, which cannot be enforced \
             from an inline bundle on the Cloudflare Worker: they require a live cross-request \
             state backend (spend/rate counters and an admission ledger) that an inline bundle \
             does not have, so they would evaluate to `allow` on every request and `failClosed` \
             could not be honored. Configure the platform bridge (NOVEUM_API_KEY + \
             NOVEUM_GUARD_PROJECT_ID), which enforces them atomically across every PoP, or remove \
             them from the inline bundle.",
        );
    }

    let strict_input_required = engine.requires_bounded_json_input(None);
    // A non-JSON request has no trustworthy model or token estimate. Every
    // stateful request is routed through atomic admission, so forwarding this
    // shape would silently bypass cost/rate counters.
    if stateful && !is_json_req {
        return if strict_input_required {
            invalid_strict_input(
                "a strict Nova Guard cost cap supports only JSON /v1/chat/completions requests",
            )
        } else {
            invalid_stateful_input(
                "Nova Guard cost/rate policies support only JSON /v1 requests because opaque bodies cannot be admitted and metered safely",
            )
        };
    }

    // --- Request body, under a hard cap ------------------------------------
    //
    // Check the declared length before reading a byte, then enforce the same
    // bound incrementally so a chunked body with no (or a lying) Content-Length
    // cannot allocate the isolate to death.
    let declared_len = req.headers().get("content-length").ok().flatten();
    if admit_request_body(declared_len.as_deref(), MAX_BODY) == BodyAdmission::TooLarge {
        return error_response(
            413,
            "invalid_request_error",
            &format!("request body exceeds the {MAX_BODY}-byte gateway limit"),
        );
    }
    let body_bytes = match read_request_body_capped(&mut req).await {
        Ok(b) => b,
        Err(()) => {
            return error_response(
                413,
                "invalid_request_error",
                &format!("request body exceeds the {MAX_BODY}-byte gateway limit"),
            )
        }
    };

    // Parse JSON when we need to inspect (guard on), meter (admission), or
    // transform it (Bedrock converts OpenAI→Converse). A transparent proxy
    // forwards bytes byte-for-byte.
    let mut body_json: Option<Value> =
        if (guard_active || stateful || is_bedrock || is_anthropic) && is_json_req {
            serde_json::from_slice(&body_bytes).ok()
        } else {
            None
        };
    if stateful && body_json.is_none() {
        return if strict_input_required {
            invalid_strict_input("a strict Nova Guard cost cap requires a valid JSON request body")
        } else {
            invalid_stateful_input(
                "Nova Guard cost/rate policies require a valid JSON request body for admission and metering",
            )
        };
    }
    let model = body_json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    if stateful && model.trim().is_empty() {
        return if strict_input_required {
            invalid_strict_input(
                "a strict Nova Guard cost cap requires a non-empty model so policy scope and pricing can be resolved",
            )
        } else {
            invalid_stateful_input(
                "Nova Guard cost/rate policies require a non-empty model for admission and metering",
            )
        };
    }

    let bedrock_creds = if is_bedrock {
        let Some(credentials) = bedrock_credentials(&req) else {
            return error_response(
                400,
                "invalid_request_error",
                "Bedrock requires x-aws-access-key-id and x-aws-secret-access-key headers",
            );
        };
        Some(credentials)
    } else {
        None
    };
    if !is_anthropic && !is_bedrock {
        let authorization = req.headers().get("authorization").ok().flatten();
        let bearer = authorization
            .as_deref()
            .and_then(authorization_bearer_token);
        if bearer.is_none() {
            return error_response(
                401,
                "authentication_error",
                "missing or invalid provider API key",
            );
        }
    }

    // Reject provider-local client errors before creating an admission hold.
    // These failures prove the provider was never called, so reserving and then
    // conservatively abandoning would consume cap headroom for invalid traffic.
    let anthropic_api_key = if is_anthropic {
        let bearer_key = req
            .headers()
            .get("authorization")
            .ok()
            .flatten()
            .and_then(|auth| authorization_bearer_token(&auth).map(str::to_string));
        let native_key = req
            .headers()
            .get("x-api-key")
            .ok()
            .flatten()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        let Some(api_key) = bearer_key.or(native_key) else {
            return error_response(
                401,
                "authentication_error",
                "missing or invalid Anthropic API key",
            );
        };
        let Some(body) = body_json.as_ref() else {
            return error_response(
                400,
                "invalid_request_error",
                "Anthropic requires a JSON chat-completions request body",
            );
        };
        if let Err(message) = openai_to_anthropic_messages_with_assumed_output_tokens(
            body.clone(),
            assumed_output_tokens,
        ) {
            return error_response(400, "invalid_request_error", &message);
        }
        Some(api_key)
    } else {
        None
    };

    // Build the exact candidate that will be forwarded before admission.
    // Policy replacements and forced stream usage can expand the body; a
    // strict hold must reserve that post-transform request, not the caller's
    // smaller original.
    let mut forward_body_json = body_json.clone();
    if guard_active {
        if let Some(candidate) = forward_body_json.as_mut() {
            apply_input_transforms(&engine, &model, candidate);
        }
    }
    if bridge.is_some() && stateful {
        if let Some(candidate) = forward_body_json.as_mut() {
            force_include_usage(&provider, candidate);
        }
    }

    // The Worker sends every stateful request through platform admission, but
    // only a model-matching enforcing/blocking strict cap needs the bounded
    // input/output contract. Advisory, shadow, flag-only and out-of-scope caps
    // retain their broader compatibility surface.
    let strict_cost_cap = engine.requires_explicit_output_limit(&model, None);
    let mut maximum_output_tokens: Option<u64> = None;
    let mut est_input_tokens: Option<u32> = None;
    if bridge.is_some() && stateful {
        if let Some(original) = body_json.as_ref() {
            let max_out = match resolve_max_output_tokens(original) {
                Ok(value) => value,
                Err(key) => return invalid_output_limit(key),
            };
            if strict_cost_cap && max_out.is_none() {
                console_warn!("Nova Guard: rejecting an unbounded request under a strict cost cap");
                return missing_strict_output_limit();
            }
            maximum_output_tokens = max_out;
        }
        if let Some(candidate) = forward_body_json.as_mut() {
            let estimate = if strict_cost_cap {
                if provider.eq_ignore_ascii_case("openai") {
                    let override_base = env_value(&env, OPENAI_BASE_URL_VAR);
                    if let Err(message) = validate_strict_openai_base_url(override_base.as_deref())
                    {
                        return invalid_strict_input(&message);
                    }
                } else if provider.eq_ignore_ascii_case("anthropic") {
                    let override_base = env_value(&env, ANTHROPIC_BASE_URL_VAR);
                    if let Err(message) =
                        validate_strict_anthropic_base_url(override_base.as_deref())
                    {
                        return invalid_strict_input(&message);
                    }
                }
                match prepare_strict_admission_body(&provider, &path, candidate) {
                    Ok(tokens) => tokens,
                    Err(message) => return invalid_strict_input(&message),
                }
            } else {
                estimate_admission_input_tokens(candidate, false)
            };
            est_input_tokens = Some(estimate);
        }
    }

    // Re-validate the exact transformed Anthropic request before taking a
    // hold. A deterministic converter error proves no upstream call occurred.
    if is_anthropic {
        if let Some(candidate) = forward_body_json.as_ref() {
            if let Err(message) = openai_to_anthropic_messages_with_assumed_output_tokens(
                candidate.clone(),
                assumed_output_tokens,
            ) {
                return error_response(400, "invalid_request_error", &message);
            }
        }
    }

    let body_rewritten = forward_body_json != body_json;
    if body_rewritten {
        let serialized = match forward_body_json.as_ref().map(serde_json::to_vec) {
            Some(Ok(body)) => body,
            Some(Err(error)) => {
                return invalid_strict_input(&format!(
                    "request body could not be serialized after Nova Guard transforms: {error}"
                ))
            }
            None => Vec::new(),
        };
        if serialized.len() > MAX_BODY {
            return transformed_body_too_large();
        }
    }

    // --- Admission + live state (platform-managed, stateful policies only) --
    let mut live_state: Option<LiveState> = None;
    if let (Some(cfg), true) = (bridge.as_ref(), stateful) {
        live_state = worker_remote::live_state(cfg).await;

        if let (Some(candidate), Some(est_in)) = (&forward_body_json, est_input_tokens) {
            // Reserve every billable request declaration as well as tokens, in
            // exact parity with native admission. Unknown models still take
            // the defensive catalog maximum rather than $0.
            let declared_usage =
                crate::policy::pricing::declared_request_usage(&provider, candidate, est_in);
            let reservation_output_tokens = maximum_output_tokens.unwrap_or(assumed_output_tokens);
            let est_cost = crate::policy::pricing::reserve_request_breakdown(
                &model,
                est_in,
                Some(reservation_output_tokens),
                &declared_usage,
            )
            .total_usd;
            let request = AdmitRequest {
                // Fresh idempotency key per logical request; the client's own
                // retries inside `admit` reuse it, so a transport failure
                // replays the same reservation instead of reserving twice.
                request_id: new_event_id(),
                provider: Some(provider.clone()),
                model: model.clone(),
                estimated_input_tokens: u64::from(est_in),
                maximum_output_tokens: reservation_output_tokens,
                estimated_cost_usd: est_cost,
                pricing_version: Some(crate::policy::pricing::CATALOG_VERSION.to_string()),
                // The Worker follows each policy's own enforcementMode. The
                // deployment-wide strict override is native-only.
                force_strict_cost_caps: false,
            };
            match worker_remote::admit(cfg, &request).await {
                Admission::Allowed(res) => {
                    *pending = Some(Pending {
                        cfg: cfg.clone(),
                        id: res.id,
                        model: model.clone(),
                        provider: provider.clone(),
                        upstream_deadline_ms: reserved_upstream_deadline_ms(&env),
                    });
                }
                Admission::Blocked(decision) => {
                    let block = decision.to_policy_decision();
                    console_log!(
                        "Nova Guard blocked request (platform admission): policy={} reason={}",
                        block.policy_id,
                        block.reason
                    );
                    // No reservation was taken, and the platform already holds
                    // the record of the block — nothing to settle or report.
                    return guard_block_response(&provider, &model, &block, block_mode);
                }
                Admission::Unavailable(reason) => {
                    // Never an implicit allow for caps configured strict. The
                    // Worker also sends advisory stateful traffic through the
                    // bridge, but a transport optimization must not change an
                    // advisory policy's fail-safety semantics.
                    match engine.admission_unavailable_decision(&reason, &model, false) {
                        Some(d) if d.is_blocking() => {
                            console_warn!(
                                "Nova Guard: platform admission unavailable ({reason}); failing closed"
                            );
                            return guard_block_response(&provider, &model, &d, block_mode);
                        }
                        Some(_) => console_warn!(
                            "Nova Guard: platform admission unavailable ({reason}); failing open"
                        ),
                        None => console_warn!(
                            "Nova Guard: platform admission unavailable ({reason}) and no \
                             applicable stateful policy was routed through admission"
                        ),
                    }
                }
            }
        }
    }

    // --- Nova Guard input phase (block, then forward prepared candidate) ----
    if guard_active {
        if let Some(j) = &body_json {
            let input_text = flatten_input_text(j);
            let result = engine.evaluate(
                Phase::Input,
                &model,
                &input_text,
                Some(j),
                est_input_tokens,
                live_state.as_ref(),
            );
            if let Some(block) = &result.block {
                // The call provably never reached the provider, so `cancel`
                // (which RELEASES the hold) is correct here — and this is the
                // only place it is.
                if let Some(p) = pending.take() {
                    p.settle(
                        ctx,
                        Settlement::Cancel(format!(
                            "blocked by gateway policy {}",
                            block.policy_id
                        )),
                    );
                }
                console_log!(
                    "Nova Guard blocked request (input phase): policy={} reason={}",
                    block.policy_id,
                    block.reason
                );
                return guard_block_response(&provider, &model, block, block_mode);
            }
        }
    }
    body_json = forward_body_json;

    // --- Build the outbound request (provider-specific routing/auth/body) ---
    let url: String;
    let out_headers: Headers;
    let forward_bytes: Vec<u8>;

    if is_bedrock {
        // AWS Bedrock Converse, signed with SigV4 (temporary creds supported).
        let creds = bedrock_creds
            .as_ref()
            .expect("Bedrock credentials were validated before admission");
        let Some(body) = body_json.as_ref() else {
            return Response::error("Bedrock requires a JSON chat-completions body", 400);
        };
        let effective_model = if model.is_empty() {
            BEDROCK_DEFAULT_MODEL
        } else {
            model.as_str()
        };
        forward_bytes = serde_json::to_vec(&openai_to_bedrock_converse(body)).unwrap_or_default();

        let host = format!("bedrock-runtime.{}.amazonaws.com", creds.region);
        // Wire path keeps the literal `:` (AWS re-encodes it); ARN `/` → %2F so
        // the id stays one segment. The SigV4 canonical URI must additionally
        // encode `:` → %3A to match AWS's canonicalization (what `aws-sigv4` does
        // internally on the native path).
        let wire_uri = format!("/model/{}/converse", effective_model.replace('/', "%2F"));
        let canonical_uri = wire_uri.replace(':', "%3A");
        url = format!("https://{host}{wire_uri}");

        let (amz_date, datestamp) = amz_timestamps();
        let signed = sigv4::sign(
            "POST",
            &host,
            &canonical_uri,
            "",
            &forward_bytes,
            &creds.access_key,
            &creds.secret_key,
            creds.session_token.as_deref(),
            &creds.region,
            "bedrock",
            &amz_date,
            &datestamp,
        );
        out_headers = Headers::new();
        out_headers.set("content-type", "application/json")?;
        out_headers.set("x-amz-date", &signed.amz_date)?;
        out_headers.set("authorization", &signed.authorization)?;
        if let Some(token) = &signed.security_token {
            out_headers.set("x-amz-security-token", token)?;
        }
    } else if is_anthropic {
        // Anthropic uses x-api-key + anthropic-version, not Bearer auth.
        out_headers = copy_headers_excluding(req.headers(), REQUEST_SKIP_HEADERS)?;
        out_headers.delete("authorization")?;
        out_headers.set("anthropic-version", "2023-06-01")?;
        let api_key = anthropic_api_key
            .as_deref()
            .expect("Anthropic authentication was validated before admission");
        out_headers.set("x-api-key", api_key)?;
        let Some(body) = body_json.take() else {
            return error_response(
                400,
                "invalid_request_error",
                "Anthropic requires a JSON chat-completions request body",
            );
        };
        let body = match openai_to_anthropic_messages_with_assumed_output_tokens(
            body,
            assumed_output_tokens,
        ) {
            Ok(body) => body,
            Err(message) => return error_response(400, "invalid_request_error", &message),
        };
        forward_bytes = serde_json::to_vec(&body).map_err(Error::from)?;

        let base = normalize_base_url(env_value(&env, ANTHROPIC_BASE_URL_VAR).as_deref())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let messages_url = format!("{base}/v1/messages");
        url = match &query {
            Some(q) => format!("{messages_url}?{q}"),
            None => messages_url,
        };
    } else {
        let route = route.expect("checked above");
        out_headers = copy_headers_excluding(req.headers(), REQUEST_SKIP_HEADERS)?;
        // `OPENAI_BASE_URL` targets a compatible upstream, the same override the
        // native gateway honors. Scoped to `x-provider: openai` for the same
        // reason it is there: it is the OpenAI SDK convention, not a general
        // per-provider redirect.
        let base_override = provider
            .eq_ignore_ascii_case("openai")
            .then(|| normalize_base_url(env_value(&env, OPENAI_BASE_URL_VAR).as_deref()))
            .flatten();
        let base = upstream_url_with_base(&route, &path, base_override.as_deref());
        url = match &query {
            Some(q) => format!("{base}?{q}"),
            None => base,
        };
        forward_bytes = forward_body_bytes(body_rewritten, &body_json, &body_bytes);
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(out_headers);
    let arr = js_sys::Uint8Array::new_with_length(forward_bytes.len() as u32);
    arr.copy_from(&forward_bytes);
    init.with_body(Some(arr.into()));
    let out_req = Request::new_with_init(&url, &init)?;
    // If this fails, `handle`'s backstop abandons the reservation.
    let fetch_request = Fetch::Request(out_req);
    let fetch = fetch_request.send();
    let mut resp = match pending.as_ref() {
        Some(hold) => match before_upstream_deadline(hold.upstream_deadline_ms, fetch).await {
            Some(result) => result?,
            None => {
                return error_response(
                    504,
                    "gateway_timeout",
                    "upstream response headers exceeded the NovaGuard reservation lease budget",
                )
            }
        },
        None => fetch.await?,
    };

    // --- Response phase ----------------------------------------------------
    let is_stream = resp
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    // Streaming remains incremental. Anthropic SSE is translated through the
    // shared state machine into OpenAI chunks; all other provider bytes pass
    // through unchanged. A held reservation tees the resulting bytes so the
    // terminal usage frame can reconcile it without buffering.
    if is_stream {
        let translate = is_anthropic && (200..300).contains(&resp.status_code());
        return stream_passthrough(resp, pending.take(), ctx, translate);
    }

    // Non-streaming: buffer only when we must transform (Anthropic/Bedrock), run
    // the output phase, or recover usage for a held reservation.
    if !is_anthropic && !is_bedrock && !guard_active && pending.is_none() {
        return passthrough(resp);
    }

    // Fast path: a declared length over the cap → pass through unbuffered. Usage
    // is then unrecoverable, so a held reservation keeps its estimate.
    let declared = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok());
    if matches!(declared, Some(n) if n > MAX_BODY) {
        if let Some(p) = pending.take() {
            p.settle(
                ctx,
                Settlement::Abandon(
                    "response exceeded the gateway inspection limit; usage not recoverable"
                        .to_string(),
                ),
            );
        }
        return passthrough(resp);
    }

    let status = resp.status_code();
    // Preserve upstream response headers (x-request-id, rate-limit, …) on the
    // re-emitted body; drop length/encoding since the body changes.
    let resp_headers = copy_headers_excluding(resp.headers(), RESPONSE_SKIP_HEADERS)?;

    // Read with a hard cap (covers chunked / missing Content-Length).
    let read = read_body_capped(&mut resp);
    let read_result = match pending.as_ref() {
        Some(hold) => match before_upstream_deadline(hold.upstream_deadline_ms, read).await {
            Some(result) => result,
            None => {
                return error_response(
                    504,
                    "gateway_timeout",
                    "upstream response body exceeded the NovaGuard reservation lease budget",
                )
            }
        },
        None => read.await,
    };
    let bytes = match read_result {
        Ok(body) => body,
        Err(()) => {
            if let Some(p) = pending.take() {
                p.settle(
                    ctx,
                    Settlement::Abandon(
                        "response exceeded the gateway inspection limit; usage not recoverable"
                            .to_string(),
                    ),
                );
            }
            return error_response(
                502,
                "gateway_error",
                "upstream response exceeded gateway inspection limit",
            );
        }
    };

    let mut out_json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        // Not JSON — return the buffered bytes unchanged, preserving headers/status.
        Err(_) => {
            if let Some(p) = pending.take() {
                p.settle_from_body(ctx, None, None);
            }
            return Ok(Response::from_bytes(bytes)?
                .with_headers(resp_headers)
                .with_status(status));
        }
    };

    // Read usage from the provider's own shape before any conversion, so a
    // dialect the OpenAI translation drops (Bedrock's camelCase counters) is
    // still recoverable; the converted body is preferred when it carries usage.
    let raw_usage = extract_actual_usage_priced(&model, &provider, &out_json);

    // Convert provider-native responses to OpenAI shape for parity (only on 2xx;
    // error envelopes pass through unchanged).
    if (200..300).contains(&status) {
        let created = (Date::now().as_millis() / 1000) as i64;
        if is_bedrock {
            let model_name = if model.is_empty() {
                BEDROCK_DEFAULT_MODEL
            } else {
                model.as_str()
            };
            out_json = bedrock_converse_to_openai(&out_json, model_name, created);
        } else if is_anthropic {
            out_json = transform_anthropic_to_openai_format(out_json, created);
        }
    }

    // Settle before the output phase can return early: the call reached the
    // provider either way, so the reservation is reconciled from whatever usage
    // the response carried, never cancelled.
    if let Some(p) = pending.take() {
        p.settle_from_body(ctx, Some(&out_json), raw_usage);
    }

    // Output phase — every edge response is OpenAI-shaped at this point.
    if guard_active {
        let output_text = flatten_output_text("openai", &out_json);
        if !output_text.is_empty() {
            let result = engine.evaluate(
                Phase::Output,
                &model,
                &output_text,
                Some(&out_json),
                None,
                live_state.as_ref(),
            );
            if let Some(block) = &result.block {
                return guard_block_response("openai", &model, block, block_mode);
            }
            if result.transformed_text.is_some() {
                apply_output_transforms(&engine, &model, "openai", &mut out_json);
            }
        }
    }

    let out_bytes = serde_json::to_vec(&out_json).unwrap_or_default();
    Ok(Response::from_bytes(out_bytes)?
        .with_headers(resp_headers)
        .with_status(status))
}
