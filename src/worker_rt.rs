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
//! (`x-aws-session-token`). Streaming responses pass through without output‑phase
//! enforcement (the documented v1 limitation, identical to native). Non‑JSON
//! `/v1/*` bodies are forwarded byte‑for‑byte.
//!
//! # Nova Guard scope on this target
//!
//! The Worker supports **stateless, inline** Nova Guard only: the text rules
//! (`regex_match`, `pii_detection`, …) from a `NOVEUM_GUARD_POLICIES` bundle,
//! which are decided entirely from the request/response payload in front of us.
//!
//! Everything that needs cross-request state is **explicitly out of scope for
//! this deployment target** and is refused with a 503 rather than silently
//! ignored:
//!
//! - **Platform-managed Nova Guard** (`NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID`):
//!   remote policy fetch, live cost/rate state, usage reporting and the
//!   admission ledger are native-only — none of that machinery is compiled for
//!   `wasm32`.
//! - **Inline `cost_cap` / `rate_limit` policies**: without a live-state backend
//!   they can only ever evaluate to "allow", which also neutralizes their
//!   `failClosed` setting — an operator would believe a hard cap is in force
//!   while every request passes.
//!
//! Supporting either here needs a Worker-native state plane (a Durable Object is
//! the natural home for atomic reservation/reconciliation) plus a `wasm32` HTTP
//! path to the Noveum API. That is deliberately not part of this change; use the
//! native gateway for platform-managed Nova Guard.

use futures_util::StreamExt;
use serde_json::{json, Value};
use worker::*;

use crate::policy::decision::{Phase, PolicyDecision};
use crate::policy::synthetic::{block_body, block_status, policy_header_token, BlockResponseMode};
use crate::policy::PolicyEngine;
use crate::routing::{
    apply_input_transforms, apply_output_transforms, bedrock_converse_to_openai,
    flatten_input_text, flatten_output_text, openai_to_bedrock_converse, resolve_provider,
    transform_anthropic_to_openai_format, upstream_url,
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

/// The bytes to forward upstream: redacted JSON re-serialized, else the original
/// request bytes verbatim.
fn forward_body_bytes(redacted: bool, body_json: &Option<Value>, original: &[u8]) -> Vec<u8> {
    if redacted {
        if let Some(j) = body_json {
            return serde_json::to_vec(j).unwrap_or_else(|_| original.to_vec());
        }
    }
    original.to_vec()
}

/// Max response size we will buffer for output-phase inspection. Larger
/// responses pass through uninspected (matches native; avoids OOM on the
/// 128 MB isolate).
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

/// Build the Nova Guard engine from an in‑memory bundle.
///
/// On the edge, policies come from the `NOVEUM_GUARD_POLICIES` Worker var/secret
/// (inline JSON) or KV (future); there is no filesystem. Absent/invalid config
/// degrades to a transparent pass‑through, exactly like the native `from_env`.
/// Honors `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` the same way `PolicyEngine::from_env`
/// does, so block shapes match the native server.
fn build_engine(env: &Env) -> PolicyEngine {
    use crate::policy::config::PolicyBundle;
    use crate::policy::engine::EngineOptions;

    let enabled = env
        .var("NOVEUM_GUARD_ENABLED")
        .map(|v| {
            !matches!(
                v.to_string().trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off" | "disabled" | ""
            )
        })
        .unwrap_or(true);

    let block_mode = env
        .var("NOVEUM_GUARD_BLOCK_RESPONSE_MODE")
        .map(|v| BlockResponseMode::from_env_str(&v.to_string()))
        .unwrap_or(BlockResponseMode::SyntheticSuccess);

    let opts = EngineOptions {
        enabled,
        block_mode,
        ..Default::default()
    };

    let bundle = env
        .var("NOVEUM_GUARD_POLICIES")
        .ok()
        .map(|v| v.to_string())
        .and_then(|s| PolicyBundle::from_json_str(&s).ok())
        .unwrap_or_default();

    PolicyEngine::from_bundle(&bundle, opts)
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

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    // CORS preflight — mirror the native permissive CorsLayer.
    if req.method() == Method::Options {
        return Ok(apply_cors(Response::empty()?.with_status(204)));
    }
    let resp = handle(req, env, ctx).await?;
    Ok(apply_cors(resp))
}

/// 503 for a Nova Guard configuration this deployment target cannot enforce.
/// Failing loudly is the point: a silent no-op would leave the operator
/// believing a cap or a fail-closed policy is in force.
fn unsupported_guard_config(message: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    Ok(Response::from_json(&json!({
        "error": {
            "message": message,
            "type": "gateway_configuration_error"
        }
    }))?
    .with_headers(headers)
    .with_status(503))
}

async fn handle(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
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

    // Platform-managed Nova Guard (remote policy fetch, live cost/rate state,
    // usage reporting) is native-only: none of that machinery is compiled for
    // wasm32, so honoring these vars here would silently proxy traffic with ZERO
    // enforcement or metering while the operator believes the platform bridge is
    // active. Refuse loudly instead of failing open — see the module docs for
    // why this target is scoped to stateless inline policies.
    let remote_configured = ["NOVEUM_API_KEY", "NOVEUM_GUARD_PROJECT_ID"]
        .iter()
        .all(|k| {
            env.secret(k)
                .map(|v| !v.to_string().trim().is_empty())
                .or_else(|_| env.var(k).map(|v| !v.to_string().trim().is_empty()))
                .unwrap_or(false)
        });
    if remote_configured {
        return unsupported_guard_config(
            "platform-managed Nova Guard (NOVEUM_API_KEY/NOVEUM_GUARD_PROJECT_ID) is not supported on the Cloudflare Worker deployment: remote policy fetch, live cost/rate state, usage reporting and the admission ledger are native-only. Unset these vars and use an inline NOVEUM_GUARD_POLICIES bundle of stateless text policies, or deploy the native gateway.",
        );
    }

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

    let engine = build_engine(&env);
    // An inline bundle may still carry `cost_cap` / `rate_limit` policies. They
    // need a live cross-request state backend, which this target does not have,
    // so they would evaluate to "allow" on every request — and `failClosed`
    // would be neutralized along with them. Refuse the request rather than let
    // an operator believe a hard cap is being enforced at the edge.
    if engine.is_enabled() && engine.stateful_policy_count() > 0 {
        return unsupported_guard_config(
            "NOVEUM_GUARD_POLICIES contains cost_cap/rate_limit policies, which are not supported on the Cloudflare Worker deployment: they require a live cross-request state backend (spend/rate counters and an admission ledger) that this target does not have, so they cannot be enforced and `failClosed` cannot be honored. Remove them from the inline bundle, or deploy the native gateway with platform-managed Nova Guard.",
        );
    }
    let guard_active = engine.is_enabled() && engine.active_policy_count() > 0;
    let block_mode = engine.block_mode();

    // Read the raw request body once.
    let body_bytes = req.bytes().await.unwrap_or_default();

    // Parse JSON when we need to inspect (guard on) or transform it (Bedrock
    // converts OpenAI→Converse). A transparent proxy forwards bytes byte-for-byte.
    let mut body_json: Option<Value> = if (guard_active || is_bedrock) && is_json_req {
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

    // --- Nova Guard input phase (block, then redact/mask transforms) ---
    let mut redacted = false;
    if guard_active {
        if let Some(j) = &body_json {
            let input_text = flatten_input_text(j);
            let result = engine.evaluate(Phase::Input, &model, &input_text, Some(j), None, None);
            if let Some(block) = &result.block {
                return guard_block_response(&provider, &model, block, block_mode);
            }
        }
        if let Some(j) = body_json.as_mut() {
            redacted = apply_input_transforms(&engine, &model, j);
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
        forward_bytes = forward_body_bytes(redacted, &body_json, &body_bytes);
    } else {
        let route = route.expect("checked above");
        out_headers = copy_headers_excluding(req.headers(), REQUEST_SKIP_HEADERS)?;
        let base = upstream_url(&route, &path);
        url = match &query {
            Some(q) => format!("{base}?{q}"),
            None => base,
        };
        forward_bytes = forward_body_bytes(redacted, &body_json, &body_bytes);
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(out_headers);
    let arr = js_sys::Uint8Array::new_with_length(forward_bytes.len() as u32);
    arr.copy_from(&forward_bytes);
    init.with_body(Some(arr.into()));
    let out_req = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(out_req).send().await?;

    // --- Response phase ---
    let is_stream = resp
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    // Streaming passes through untouched (matches native v1; Anthropic SSE stays
    // in Anthropic shape, exactly like the native server).
    if is_stream {
        return passthrough(resp);
    }

    // Non-streaming: only buffer when we must transform (Anthropic/Bedrock) or
    // run the output phase; otherwise stream the upstream response straight back.
    if !is_anthropic && !is_bedrock && !guard_active {
        return passthrough(resp);
    }

    // Fast path: a declared length over the cap → pass through unbuffered.
    let declared = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok());
    if matches!(declared, Some(n) if n > MAX_BODY) {
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
            let headers = Headers::new();
            headers.set("content-type", "application/json")?;
            return Ok(Response::from_json(&json!({
                "error": {"message": "upstream response exceeded gateway inspection limit", "type": "gateway_error"}
            }))?
            .with_headers(headers)
            .with_status(502));
        }
    };

    let mut out_json: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        // Not JSON — return the buffered bytes unchanged, preserving headers/status.
        Err(_) => {
            return Ok(Response::from_bytes(bytes)?
                .with_headers(resp_headers)
                .with_status(status))
        }
    };

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
                None,
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
