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
//! (`NOVEUM_GUARD_BLOCK_RESPONSE_MODE`), and the same Anthropic→OpenAI response
//! conversion. Streaming responses pass through without output‑phase enforcement
//! (the documented v1 limitation, identical to native). Non‑JSON `/v1/*` bodies
//! are forwarded byte‑for‑byte. Bedrock (AWS SigV4 via Web Crypto) lands later.

use futures_util::StreamExt;
use serde_json::{json, Value};
use worker::*;

use crate::policy::decision::{Phase, PolicyDecision};
use crate::policy::synthetic::{block_body, block_status, policy_header_token, BlockResponseMode};
use crate::policy::PolicyEngine;
use crate::routing::{
    apply_input_transforms, apply_output_transforms, flatten_input_text, flatten_output_text,
    resolve_provider, transform_anthropic_to_openai_format, upstream_url,
};

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

    let provider = req
        .headers()
        .get("x-provider")
        .ok()
        .flatten()
        .unwrap_or_else(|| "openai".to_string());
    let is_anthropic = provider.eq_ignore_ascii_case("anthropic");
    let route = resolve_provider(&provider);
    if route.is_none() && !is_anthropic {
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
    let guard_active = engine.is_enabled() && engine.active_policy_count() > 0;
    let block_mode = engine.block_mode();

    // Read the raw request body once.
    let body_bytes = req.bytes().await.unwrap_or_default();

    // Parse JSON only when we actually need to inspect it (guard on). A
    // transparent proxy forwards the original bytes byte-for-byte.
    let mut body_json: Option<Value> = if guard_active && is_json_req {
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

    // --- Build the outbound request (forward client headers + provider auth) ---
    let out_headers = copy_headers_excluding(req.headers(), REQUEST_SKIP_HEADERS)?;
    let url = if is_anthropic {
        // Anthropic uses x-api-key + anthropic-version, not Bearer auth.
        out_headers.delete("authorization")?;
        out_headers.set("anthropic-version", "2023-06-01")?;
        if let Ok(Some(auth)) = req.headers().get("authorization") {
            out_headers.set("x-api-key", auth.trim_start_matches("Bearer ").trim())?;
        }
        "https://api.anthropic.com/v1/messages".to_string()
    } else {
        let route = route.expect("checked above");
        let base = upstream_url(&route, &path);
        match &query {
            Some(q) => format!("{base}?{q}"),
            None => base,
        }
    };

    // Forward the redacted JSON (re-serialized) or the original bytes verbatim.
    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(out_headers);
    if redacted {
        if let Some(j) = &body_json {
            init.with_body(Some(serde_json::to_string(j).unwrap_or_default().into()));
        }
    } else {
        let arr = js_sys::Uint8Array::new_with_length(body_bytes.len() as u32);
        arr.copy_from(&body_bytes);
        init.with_body(Some(arr.into()));
    }
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

    // Non-streaming: only buffer when we must transform (Anthropic) or run the
    // output phase; otherwise stream the upstream response straight back.
    if !is_anthropic && !guard_active {
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

    // Anthropic → OpenAI shape for parity (only successful responses carry the
    // Messages body; errors pass through as-is).
    if is_anthropic && (200..300).contains(&status) {
        let created = (Date::now().as_millis() / 1000) as i64;
        out_json = transform_anthropic_to_openai_format(out_json, created);
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
