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

pub async fn guard_middleware(
    State(engine): State<Arc<PolicyEngine>>,
    req: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
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

    let mut forward_bytes = bytes.clone();

    if let Some(mut body_json) = json.clone() {
        let input_text = flatten_input_text(&body_json);

        let result = engine.evaluate(
            Phase::Input,
            &model,
            &input_text,
            Some(&body_json),
            None,
            None, // standalone: no live cost/rate state (control-plane path supplies it)
        );

        log_decisions("input", &provider, &model, &result.decisions);

        if let Some(block) = &result.block {
            info!(
                provider = %provider, model = %model, policy = %block.policy_id,
                reason = %block.reason, "Nova Guard blocked request (input phase)"
            );
            return block_response(&provider, &model, block, engine.block_mode());
        }

        // Apply input transforms per text segment (so structured messages stay valid).
        if result.transformed_text.is_some()
            && apply_input_transforms(&engine, &model, &mut body_json)
        {
            if let Ok(v) = serde_json::to_vec(&body_json) {
                forward_bytes = v.into();
            }
        }
    }

    let forwarded = Request::from_parts(parts, Body::from(forward_bytes));
    let response = next.run(forwarded).await;

    // --- OUTPUT PHASE ---
    enforce_output(&engine, &provider, &model, response).await
}

/// Should this request be inspected? POST, JSON, on the `/v1/` proxy path.
fn is_guardable<B>(req: &Request<B>) -> bool {
    if req.method() != axum::http::Method::POST {
        return false;
    }
    if !req.uri().path().contains("/v1/") {
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

/// Flatten the user-supplied input text from a chat/completions-style body.
///
/// Handles OpenAI/Anthropic `messages[].content` (string or array-of-parts),
/// Anthropic top-level `system`, and a plain `prompt` string.
pub fn flatten_input_text(json: &Value) -> String {
    let mut out = String::new();

    if let Some(system) = json.get("system").and_then(|s| s.as_str()) {
        out.push_str(system);
        out.push('\n');
    }

    if let Some(prompt) = json.get("prompt").and_then(|p| p.as_str()) {
        out.push_str(prompt);
        out.push('\n');
    }

    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            match msg.get("content") {
                Some(Value::String(s)) => {
                    out.push_str(s);
                    out.push('\n');
                }
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                            out.push('\n');
                        }
                    }
                }
                _ => {}
            }
        }
    }

    out
}

/// Apply input transforms to each string segment in the body in place.
/// Returns true if anything was mutated.
fn apply_input_transforms(engine: &PolicyEngine, model: &str, json: &mut Value) -> bool {
    let mut changed = false;

    let transform_segment = |s: &str| -> Option<String> {
        let r = engine.evaluate(Phase::Input, model, s, None, None, None);
        r.transformed_text
    };

    if let Some(system) = json.get("system").and_then(|s| s.as_str()) {
        if let Some(t) = transform_segment(system) {
            if t != system {
                json["system"] = Value::String(t);
                changed = true;
            }
        }
    }
    if let Some(prompt) = json.get("prompt").and_then(|p| p.as_str()) {
        if let Some(t) = transform_segment(prompt) {
            if t != prompt {
                json["prompt"] = Value::String(t);
                changed = true;
            }
        }
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(Value::String(s)) = msg.get("content") {
                let s = s.clone();
                if let Some(t) = transform_segment(&s) {
                    if t != s {
                        msg["content"] = Value::String(t);
                        changed = true;
                    }
                }
            }
        }
    }

    changed
}

/// Extract the assistant text from a provider response body.
pub fn flatten_output_text(provider: &str, json: &Value) -> String {
    match provider {
        "anthropic" => json
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "google" | "gemini" => json
            .get("candidates")
            .and_then(|c| c.as_array())
            .map(|cands| {
                cands
                    .iter()
                    .filter_map(|c| c.get("content").and_then(|ct| ct.get("parts")))
                    .filter_map(|p| p.as_array())
                    .flat_map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        // OpenAI / compatible
        _ => json
            .get("choices")
            .and_then(|c| c.as_array())
            .map(|choices| {
                choices
                    .iter()
                    .filter_map(|c| {
                        c.get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    }
}

async fn enforce_output(
    engine: &PolicyEngine,
    provider: &str,
    model: &str,
    response: Response,
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

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            warn!("Nova Guard: response body too large to inspect; passing through");
            return Response::from_parts(
                parts,
                Body::from("response body exceeded inspection limit"),
            );
        }
    };

    let json: Option<Value> = serde_json::from_slice(&bytes).ok();
    let Some(body_json) = json else {
        // Not JSON (or empty) — pass through unchanged.
        return Response::from_parts(parts, Body::from(bytes));
    };

    let output_text = flatten_output_text(provider, &body_json);
    if output_text.is_empty() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let result = engine.evaluate(
        Phase::Output,
        model,
        &output_text,
        Some(&body_json),
        None,
        None,
    );
    log_decisions("output", provider, model, &result.decisions);

    if let Some(block) = &result.block {
        info!(
            provider = %provider, model = %model, policy = %block.policy_id,
            "Nova Guard blocked response (output phase)"
        );
        return block_response(provider, model, block, engine.block_mode());
    }

    // Output transform: rewrite the assistant text in place for the common shapes.
    if let Some(transformed) = &result.transformed_text {
        let mut out_json = body_json;
        if rewrite_output_text(provider, &mut out_json, transformed) {
            if let Ok(v) = serde_json::to_vec(&out_json) {
                let mut parts = parts;
                parts.headers.remove(header::CONTENT_LENGTH);
                return Response::from_parts(parts, Body::from(v));
            }
        }
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// Rewrite the first assistant text field with the transformed text. Best-effort
/// for the common OpenAI/Anthropic shapes.
fn rewrite_output_text(provider: &str, json: &mut Value, new_text: &str) -> bool {
    match provider {
        "anthropic" => {
            if let Some(parts) = json.get_mut("content").and_then(|c| c.as_array_mut()) {
                if let Some(first_text) = parts
                    .iter_mut()
                    .find(|p| p.get("text").map(|t| t.is_string()).unwrap_or(false))
                {
                    first_text["text"] = Value::String(new_text.to_string());
                    return true;
                }
            }
            false
        }
        _ => {
            if let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) {
                if let Some(first) = choices.first_mut() {
                    if let Some(msg) = first.get_mut("message") {
                        msg["content"] = Value::String(new_text.to_string());
                        return true;
                    }
                }
            }
            false
        }
    }
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
    fn rewrite_openai_output_text() {
        let mut j = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "secret stuff"}}]
        });
        assert!(rewrite_output_text("openai", &mut j, "[REDACTED]"));
        assert_eq!(j["choices"][0]["message"]["content"], "[REDACTED]");
    }

    #[test]
    fn rewrite_anthropic_output_text() {
        let mut j = serde_json::json!({"content": [{"type": "text", "text": "secret"}]});
        assert!(rewrite_output_text("anthropic", &mut j, "[X]"));
        assert_eq!(j["content"][0]["text"], "[X]");
    }
}
