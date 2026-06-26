//! Provider-shaped synthetic responses for blocked requests.
//!
//! When an `Enforce` policy blocks a call, the gateway returns a synthetic
//! response *instead of* contacting the provider. Two modes (per the Nova Guard
//! design):
//!
//! * `synthetic_success` — HTTP 200 with a normal-looking completion whose
//!   content is the block reason. The caller's SDK parses it as a successful
//!   response (graceful degradation), and crucially the status is not in any
//!   provider SDK's retry set.
//! * `provider_error` — HTTP 403 with the provider's error envelope. The
//!   caller's SDK raises a permission error. Also outside the retry set.
//!
//! An `x_noveum_guard` extension object is attached so tooling can identify
//! guard-synthesised responses; provider SDKs ignore unknown fields.

// `BlockResponseMode` + the provider-shaped block *body* builders + status are
// SHARED, so the native server and the Cloudflare Worker emit byte-identical
// block responses. Only the axum `Response` wrapper is native-only; the Worker
// wraps the same body/status in a `worker::Response`.
use super::decision::PolicyDecision;
use serde_json::{json, Value};
use uuid::Uuid;

#[cfg(not(target_arch = "wasm32"))]
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};

/// How a block should be surfaced to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockResponseMode {
    /// HTTP 200 with a synthetic completion containing the block reason.
    SyntheticSuccess,
    /// HTTP 4xx with the provider's error envelope.
    ProviderError,
}

impl BlockResponseMode {
    pub fn from_env_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "provider_error" | "error" => BlockResponseMode::ProviderError,
            _ => BlockResponseMode::SyntheticSuccess,
        }
    }
}

/// Sanitize a policy id to a safe ASCII header token (so building a response
/// header can never fail). Shared by native + Worker.
pub fn policy_header_token(policy_id: &str) -> String {
    policy_id
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '\u{7f}' {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect()
}

/// HTTP status for a block in the given mode (shared by native + Worker).
pub fn block_status(mode: BlockResponseMode) -> u16 {
    match mode {
        BlockResponseMode::ProviderError => 403,
        BlockResponseMode::SyntheticSuccess => 200,
    }
}

/// Build the provider-shaped block response *body* for the given mode. Shared by
/// native (`block_response`) and the Cloudflare Worker (`worker_rt`).
pub fn block_body(
    provider: &str,
    model: &str,
    decision: &PolicyDecision,
    mode: BlockResponseMode,
) -> Value {
    match mode {
        BlockResponseMode::ProviderError => error_body(provider, decision),
        BlockResponseMode::SyntheticSuccess => success_body(provider, model, decision),
    }
}

fn guard_extension(decision: &PolicyDecision) -> Value {
    json!({
        "blocked": true,
        "policy_id": decision.policy_id,
        "policy_name": decision.policy_name,
        "policy_type": decision.policy_type,
        "reason": decision.reason,
    })
}

/// Build a synthetic block response shaped for `provider`, in the given mode.
///
/// `provider` is the `x-provider` value (e.g. `"openai"`, `"anthropic"`,
/// `"google"`); unknown providers fall back to the OpenAI shape, which most
/// OpenAI-compatible SDKs accept.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_response(
    provider: &str,
    model: &str,
    decision: &PolicyDecision,
    mode: BlockResponseMode,
) -> Response {
    let body = block_body(provider, model, decision, mode);
    let status =
        StatusCode::from_u16(block_status(mode)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let policy_header = policy_header_token(&decision.policy_id);

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-noveum-guard-blocked", "true")
        .header("x-noveum-guard-policy", policy_header)
        .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
        .expect("synthetic response with sanitized header is always valid")
}

fn reason_text(decision: &PolicyDecision) -> String {
    format!(
        "Request blocked by Nova Guard policy '{}': {}",
        decision.policy_name, decision.reason
    )
}

fn error_body(provider: &str, decision: &PolicyDecision) -> Value {
    let message = reason_text(decision);
    match provider {
        "anthropic" => json!({
            "type": "error",
            "error": {"type": "permission_error", "message": message},
            "x_noveum_guard": guard_extension(decision),
        }),
        _ => json!({
            "error": {
                "message": message,
                "type": "guard_blocked",
                "code": "noveum_guard_blocked",
                "param": null,
            },
            "x_noveum_guard": guard_extension(decision),
        }),
    }
}

fn success_body(provider: &str, model: &str, decision: &PolicyDecision) -> Value {
    let content = reason_text(decision);
    let id_suffix = Uuid::new_v4().to_string();
    let created = 1_750_000_000u64; // fixed, monotonic-enough placeholder; not security-relevant

    match provider {
        "anthropic" => json!({
            "id": format!("msg_noveum_guard_{id_suffix}"),
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": content}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 0, "output_tokens": 0},
            "x_noveum_guard": guard_extension(decision),
        }),
        "google" | "gemini" => json!({
            "candidates": [{
                "content": {"parts": [{"text": content}], "role": "model"},
                "finishReason": "STOP",
                "index": 0,
            }],
            "usageMetadata": {"promptTokenCount": 0, "candidatesTokenCount": 0, "totalTokenCount": 0},
            "x_noveum_guard": guard_extension(decision),
        }),
        // OpenAI / OpenAI-compatible default shape.
        _ => json!({
            "id": format!("chatcmpl-noveum-guard-{id_suffix}"),
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            "x_noveum_guard": guard_extension(decision),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::decision::{PolicyMode, Severity};
    use http_body_util::BodyExt;

    fn decision() -> PolicyDecision {
        let mut d =
            PolicyDecision::allow("pol_1", "Monthly budget", "cost_cap", PolicyMode::Enforce);
        d.flagged = true;
        d.severity = Severity::Critical;
        d.reason = "spend cap reached".to_string();
        d
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn mode_from_env() {
        assert_eq!(
            BlockResponseMode::from_env_str("provider_error"),
            BlockResponseMode::ProviderError
        );
        assert_eq!(
            BlockResponseMode::from_env_str("error"),
            BlockResponseMode::ProviderError
        );
        assert_eq!(
            BlockResponseMode::from_env_str("anything"),
            BlockResponseMode::SyntheticSuccess
        );
    }

    #[tokio::test]
    async fn openai_success_shape_is_200_with_choices() {
        let resp = block_response(
            "openai",
            "gpt-4o",
            &decision(),
            BlockResponseMode::SyntheticSuccess,
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-noveum-guard-blocked").unwrap(),
            "true"
        );
        let b = body_json(resp).await;
        assert_eq!(b["object"], "chat.completion");
        assert_eq!(b["model"], "gpt-4o");
        assert!(b["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("spend cap reached"));
        assert_eq!(b["x_noveum_guard"]["blocked"], true);
    }

    #[tokio::test]
    async fn anthropic_success_shape() {
        let resp = block_response(
            "anthropic",
            "claude-opus-4-8",
            &decision(),
            BlockResponseMode::SyntheticSuccess,
        );
        let b = body_json(resp).await;
        assert_eq!(b["type"], "message");
        assert_eq!(b["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn google_success_shape() {
        let resp = block_response(
            "google",
            "gemini-2.5-pro",
            &decision(),
            BlockResponseMode::SyntheticSuccess,
        );
        let b = body_json(resp).await;
        assert!(b["candidates"][0]["content"]["parts"][0]["text"].is_string());
    }

    #[tokio::test]
    async fn provider_error_is_403() {
        let resp = block_response(
            "openai",
            "gpt-4o",
            &decision(),
            BlockResponseMode::ProviderError,
        );
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let b = body_json(resp).await;
        assert_eq!(b["error"]["code"], "noveum_guard_blocked");
    }

    #[tokio::test]
    async fn anthropic_error_envelope() {
        let resp = block_response(
            "anthropic",
            "claude-opus-4-8",
            &decision(),
            BlockResponseMode::ProviderError,
        );
        let b = body_json(resp).await;
        assert_eq!(b["error"]["type"], "permission_error");
    }

    #[tokio::test]
    async fn unknown_provider_falls_back_to_openai_shape() {
        let resp = block_response(
            "some-new-provider",
            "x",
            &decision(),
            BlockResponseMode::SyntheticSuccess,
        );
        let b = body_json(resp).await;
        assert_eq!(b["object"], "chat.completion");
    }
}
