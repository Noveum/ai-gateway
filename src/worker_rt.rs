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

use std::rc::Rc;

use futures::channel::oneshot;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use worker::*;

use crate::policy::decision::{Phase, PolicyDecision};
use crate::policy::engine::EngineOptions;
use crate::policy::metering::{extract_actual_usage, ActualUsage, StreamUsageScanner};
use crate::policy::rules::LiveState;
use crate::policy::synthetic::{block_body, block_status, policy_header_token, BlockResponseMode};
use crate::policy::worker_remote::{
    self, admit_request_body, estimate_input_tokens, force_include_usage,
    resolve_max_output_tokens, Admission, AdmitRequest, BodyAdmission, Settlement, StreamOutcome,
    WorkerRemoteConfig, ALLOW_UNGUARDED_START_VAR, API_KEY_VAR, API_URL_VAR,
    MAX_OUTPUT_TOKEN_LIMIT, PROJECT_ID_VAR, TENANCY_VAR,
};
use crate::policy::PolicyEngine;
use crate::routing::{
    apply_input_transforms, apply_output_transforms, bedrock_converse_to_openai,
    flatten_input_text, flatten_output_text, normalize_base_url, openai_to_bedrock_converse,
    resolve_provider, transform_anthropic_to_openai_format, upstream_url_with_base,
    OPENAI_BASE_URL_VAR,
};
use crate::sigv4;

/// Default Bedrock model + region (mirrors the native `BedrockProvider`).
const BEDROCK_DEFAULT_MODEL: &str = "amazon.titan-text-premier-v1:0";
const BEDROCK_DEFAULT_REGION: &str = "us-east-1";

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
    let access_key = h.get("x-aws-access-key-id").ok().flatten()?;
    let secret_key = h.get("x-aws-secret-access-key").ok().flatten()?;
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
    "x-provider",
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
    for (k, v) in src.entries() {
        if skip.contains(&k.to_ascii_lowercase().as_str()) {
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
fn tee_usage<S>(inner: S, tx: oneshot::Sender<StreamOutcome>) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin + 'static,
{
    struct Tee<S> {
        inner: S,
        scanner: StreamUsageScanner,
        tx: Option<oneshot::Sender<StreamOutcome>>,
    }

    futures_util::stream::unfold(
        Tee {
            inner,
            scanner: StreamUsageScanner::new(),
            tx: Some(tx),
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
                        let _ = tx.send(match tee.scanner.usage() {
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
fn metered_passthrough(mut resp: Response, pending: Pending, ctx: &Context) -> Result<Response> {
    let status = resp.status_code();
    let headers = copy_headers_excluding(resp.headers(), RESPONSE_SKIP_HEADERS)?;
    let stream = resp.stream()?;

    let (tx, rx) = oneshot::channel::<StreamOutcome>();
    let Pending { cfg, id, model } = pending;
    ctx.wait_until(async move {
        // `Err` = the sender was dropped with the body, i.e. the client vanished
        // before the final frame.
        let outcome = rx.await.unwrap_or(StreamOutcome::Dropped);
        let settlement = worker_remote::settlement_for(outcome, &model, Some(new_event_id()));
        worker_remote::settle(&cfg, &id, &settlement).await;
    });

    Ok(Response::from_stream(tee_usage(stream, tx))?
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
    let mut body_json: Option<Value> = if (guard_active || stateful || is_bedrock) && is_json_req {
        serde_json::from_slice(&body_bytes).ok()
    } else {
        None
    };
    let model = body_json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // --- Admission + live state (platform-managed, stateful policies only) --
    let mut live_state: Option<LiveState> = None;
    let mut est_input_tokens: Option<u32> = None;
    if let (Some(cfg), true) = (bridge.as_ref(), stateful) {
        live_state = worker_remote::live_state(cfg).await;

        if let Some(j) = &body_json {
            let est_in = estimate_input_tokens(&flatten_input_text(j));
            est_input_tokens = Some(est_in);
            // Untrusted client JSON — reject an unusable limit before it reaches
            // the admission arithmetic (or the provider).
            let max_out = match resolve_max_output_tokens(j) {
                Ok(v) => v,
                Err(key) => return invalid_output_limit(key),
            };
            // Unknown models reserve the defensive assumption, not $0 - the
            // edge must not admit what the native path would refuse.
            let est_cost = crate::policy::pricing::reserve_request_cost(&model, est_in, max_out);
            let request = AdmitRequest {
                // Fresh idempotency key per logical request; the client's own
                // retries inside `admit` reuse it, so a transport failure
                // replays the same reservation instead of reserving twice.
                request_id: new_event_id(),
                provider: Some(provider.clone()),
                model: model.clone(),
                estimated_input_tokens: u64::from(est_in),
                maximum_output_tokens: max_out
                    .unwrap_or_else(crate::policy::pricing::assumed_output_tokens),
                estimated_cost_usd: est_cost,
                pricing_version: Some(crate::policy::pricing::CATALOG_VERSION.to_string()),
            };
            match worker_remote::admit(cfg, &request).await {
                Admission::Allowed(res) => {
                    *pending = Some(Pending {
                        cfg: cfg.clone(),
                        id: res.id,
                        model: model.clone(),
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
                    // Never an implicit allow: apply `failClosed` exactly as an
                    // unavailable `/state` would. This runtime routes EVERY
                    // stateful policy through admission -- it has no in-process
                    // ledger to fall back to -- so the fail-closed branch must
                    // consider every cost cap, which is what a forced strict
                    // override selects.
                    match engine.admission_unavailable_decision(
                        &reason,
                        Some(crate::policy::config::CostEnforcementMode::Strict),
                    ) {
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
                            "Nova Guard: platform admission unavailable ({reason}) and no cost \
                             cap was routed through admission"
                        ),
                    }
                }
            }
        }
    }

    // --- Nova Guard input phase (block, then redact/mask transforms) --------
    let mut body_rewritten = false;
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
        if let Some(j) = body_json.as_mut() {
            body_rewritten = apply_input_transforms(&engine, &model, j);
        }
    }

    // While the platform is metering, ask OpenAI to append its terminal usage
    // chunk to streaming responses. Without it a stream carries no token counts
    // and every streamed request would settle at `input + max_tokens`.
    if pending.is_some() {
        if let Some(j) = body_json.as_mut() {
            if force_include_usage(&provider, j) {
                body_rewritten = true;
            }
        }
    }

    // --- Build the outbound request (provider-specific routing/auth/body) ---
    let url: String;
    let out_headers: Headers;
    let forward_bytes: Vec<u8>;

    if is_bedrock {
        // AWS Bedrock Converse, signed with SigV4 (temporary creds supported).
        let Some(creds) = bedrock_credentials(&req) else {
            return Response::error(
                "Bedrock requires x-aws-access-key-id and x-aws-secret-access-key headers",
                400,
            );
        };
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
        if let Ok(Some(auth)) = req.headers().get("authorization") {
            out_headers.set("x-api-key", auth.trim_start_matches("Bearer ").trim())?;
        }
        url = "https://api.anthropic.com/v1/messages".to_string();
        forward_bytes = forward_body_bytes(body_rewritten, &body_json, &body_bytes);
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
    let mut resp = Fetch::Request(out_req).send().await?;

    // --- Response phase ----------------------------------------------------
    let is_stream = resp
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    // Streaming passes through untouched (matches native v1; Anthropic SSE stays
    // in Anthropic shape, exactly like the native server). When a reservation is
    // held, the bytes are teed on the way past so the terminal usage frame can
    // reconcile it — still no buffering.
    if is_stream {
        return match pending.take() {
            Some(p) => metered_passthrough(resp, p, ctx),
            None => passthrough(resp),
        };
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
    let bytes = match read_body_capped(&mut resp).await {
        Ok(b) => b,
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
    let raw_usage = extract_actual_usage(&out_json);

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
