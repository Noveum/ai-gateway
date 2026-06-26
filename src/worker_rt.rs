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
//! contract, same Nova Guard input/output decisions + redaction, and the same
//! Anthropic→OpenAI response conversion. Streaming responses pass through
//! without output‑phase enforcement (the documented v1 limitation, identical to
//! native). Bedrock (AWS SigV4 via Web Crypto) lands in a later phase.

use serde_json::{json, Value};
use worker::*;

use crate::policy::{decision::Phase, PolicyEngine};
use crate::routing::{
    apply_input_transforms, flatten_input_text, flatten_output_text, resolve_provider,
    rewrite_output_text, transform_anthropic_to_openai_format, upstream_url,
};

/// Build the Nova Guard engine from an in‑memory bundle.
///
/// On the edge, policies come from the `NOVEUM_GUARD_POLICIES` Worker var/secret
/// (inline JSON) or KV (future); there is no filesystem. Absent/invalid config
/// degrades to a transparent pass‑through, exactly like the native `from_env`.
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

    let opts = EngineOptions {
        enabled,
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

/// Nova Guard block envelope (OpenAI-style error). Returned with HTTP 200 +
/// `x-noveum-guard-blocked` so clients can detect a guard block distinctly from
/// an upstream error.
fn guard_block_response(policy_name: &str, reason: &str) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    headers.set("x-noveum-guard-blocked", "true")?;
    let body = json!({
        "error": {
            "message": format!("Blocked by Nova Guard policy '{policy_name}': {reason}"),
            "type": "guard_blocked",
            "code": "noveum_guard_blocked",
        }
    });
    Ok(Response::from_json(&body)?
        .with_headers(headers)
        .with_status(200))
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
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

    // Read + parse the body once (JSON chat bodies are text).
    let body_text = req.text().await.unwrap_or_default();
    let mut body_json: Option<Value> = serde_json::from_str(&body_text).ok();
    let model = body_json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    let engine = build_engine(&env);
    let guard_active = engine.is_enabled() && engine.active_policy_count() > 0;

    // --- Nova Guard input phase (block, then redact/mask transforms) ---
    if guard_active {
        if let Some(json) = &body_json {
            let input_text = flatten_input_text(json);
            let result = engine.evaluate(Phase::Input, &model, &input_text, Some(json), None, None);
            if let Some(block) = &result.block {
                return guard_block_response(&block.policy_name, &block.reason);
            }
        }
        if let Some(json) = body_json.as_mut() {
            apply_input_transforms(&engine, &model, json);
        }
    }

    // The body to forward (carries any input redactions).
    let forward_body = match &body_json {
        Some(j) => serde_json::to_string(j).unwrap_or_else(|_| body_text.clone()),
        None => body_text.clone(),
    };

    // --- Build the outbound request (provider-specific routing/auth) ---
    let (url, out_headers) = if is_anthropic {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        headers.set("anthropic-version", "2023-06-01")?;
        if let Ok(Some(auth)) = req.headers().get("authorization") {
            let key = auth.trim_start_matches("Bearer ").trim();
            headers.set("x-api-key", key)?;
        }
        ("https://api.anthropic.com/v1/messages".to_string(), headers)
    } else {
        let route = route.expect("checked above");
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        if let Ok(Some(auth)) = req.headers().get("authorization") {
            headers.set("authorization", &auth)?;
        }
        (upstream_url(&route, &path), headers)
    };

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(out_headers)
        .with_body(Some(forward_body.into()));
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
        return Ok(resp);
    }

    // Non-streaming: only buffer when we must transform (Anthropic) or run the
    // output phase; otherwise stream the upstream response straight back.
    if !is_anthropic && !guard_active {
        return Ok(resp);
    }

    // Don't buffer responses beyond the inspection cap — pass them through
    // uninspected rather than risk OOM on the 128 MB isolate (matches native).
    const MAX_BODY: u64 = 8 * 1024 * 1024;
    let too_large = resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|n| n > MAX_BODY)
        .unwrap_or(false);
    if too_large {
        return Ok(resp);
    }

    let status = resp.status_code();
    let text = resp.text().await.unwrap_or_default();
    let mut out_json: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(Response::ok(text)?.with_status(status)),
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
                return guard_block_response(&block.policy_name, &block.reason);
            }
            if let Some(transformed) = &result.transformed_text {
                rewrite_output_text("openai", &mut out_json, transformed);
            }
        }
    }

    Ok(Response::from_json(&out_json)?.with_status(status))
}
