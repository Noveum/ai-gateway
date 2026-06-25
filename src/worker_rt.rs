//! Cloudflare Worker runtime (WASM).
//!
//! This is the `wasm32` entry point: a `#[event(fetch)]` handler that runs the
//! gateway as a true per‑PoP edge Worker. It reuses the **shared Nova Guard
//! engine** ([`crate::policy`]) for input enforcement, then proxies the request
//! to the upstream provider via the platform `Fetch` API (no `reqwest`/`tokio`).
//!
//! Parity with the native server: same provider routing, same `x-provider`
//! contract, same Nova Guard decisions. Bedrock (AWS SigV4 via Web Crypto) and
//! streaming output‑phase enforcement land in later phases.

use worker::*;

use crate::policy::{decision::Phase, PolicyEngine};
use crate::routing::{flatten_input_text, resolve_provider, upstream_url};

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

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();

    if path == "/health" {
        return Response::from_json(&serde_json::json!({
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

    let Some(route) = resolve_provider(&provider) else {
        return Response::error(
            format!("Unsupported or not-yet-ported provider on edge: {provider}"),
            400,
        );
    };

    // Read the body once (JSON chat bodies are text).
    let body_text = req.text().await.unwrap_or_default();

    // --- Nova Guard input phase (shared engine) ---
    let engine = build_engine(&env);
    if engine.is_enabled() && engine.active_policy_count() > 0 {
        if let Ok(body_json) = serde_json::from_str::<serde_json::Value>(&body_text) {
            let model = body_json
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let input_text = flatten_input_text(&body_json);
            let result = engine.evaluate(
                Phase::Input,
                &model,
                &input_text,
                Some(&body_json),
                None,
                None,
            );
            if let Some(block) = &result.block {
                let headers = Headers::new();
                headers.set("content-type", "application/json")?;
                headers.set("x-noveum-guard-blocked", "true")?;
                let body = serde_json::json!({
                    "error": {
                        "message": format!("Request blocked by Nova Guard policy '{}': {}", block.policy_name, block.reason),
                        "type": "guard_blocked",
                        "code": "noveum_guard_blocked",
                    }
                });
                return Ok(Response::from_json(&body)?
                    .with_headers(headers)
                    .with_status(200));
            }
            // (Transform/redact + output phase land in a later phase.)
        }
    }

    // --- Proxy to the upstream provider via the platform Fetch API ---
    let url = upstream_url(&route, &path);

    let out_headers = Headers::new();
    if let Ok(Some(auth)) = req.headers().get("authorization") {
        out_headers.set("authorization", &auth)?;
    }
    out_headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(out_headers)
        .with_body(Some(body_text.into()));

    let out_req = Request::new_with_init(&url, &init)?;
    Fetch::Request(out_req).send().await
}
