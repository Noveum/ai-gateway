//! End-to-end integration tests for the Nova Guard middleware.
//!
//! These drive a real Axum router (guard middleware layered over a stub upstream
//! handler) with `tower::ServiceExt::oneshot`, so they exercise the full request
//! path — body buffering, engine evaluation, blocking, transforms, and
//! pass-through — without binding a port or calling real providers.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{header, Request, StatusCode},
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use noveum_ai_gateway::policy::{
    engine::EngineOptions, middleware::guard_middleware, PolicyBundle, PolicyEngine,
};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Stub upstream: echoes the (possibly transformed) request body back as a
/// 200 JSON response. Lets us assert what the guard forwarded.
async fn echo_handler(body: Bytes) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Stub upstream that returns a canned OpenAI-shaped response containing a secret,
/// for exercising output-phase policies.
async fn canned_secret_handler() -> Response {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "your key is AKIAIOSFODNN7EXAMPLE"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap(),
    )
        .into_response()
}

fn router(engine: PolicyEngine) -> Router {
    // No platform live-state in tests (`live: None`) → cost_cap/rate_limit fail open.
    // No usage reporter either (`usage: None`).
    let guard_state = noveum_ai_gateway::policy::middleware::GuardState {
        engine: Arc::new(engine),
        live: None,
        usage: None,
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .route("/v1/canned", post(canned_secret_handler))
        .layer(from_fn_with_state(guard_state, guard_middleware))
}

fn engine_from(json_bundle: &str) -> PolicyEngine {
    let bundle = PolicyBundle::from_json_str(json_bundle).unwrap();
    PolicyEngine::from_bundle(&bundle, EngineOptions::default())
}

fn post_json(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-provider", "openai")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn response_json(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn passthrough_when_no_policy_matches() {
    let engine = engine_from(
        r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"enforce",
        "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello there"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    // echoed back unchanged
    assert_eq!(body["messages"][0]["content"], "hello there");
}

#[tokio::test]
async fn blocks_input_with_synthetic_success() {
    let engine = engine_from(
        r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"enforce",
        "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "my ssn is 123-45-6789"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get("x-noveum-guard-blocked").unwrap(),
        "true"
    );
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK); // synthetic_success default
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["x_noveum_guard"]["blocked"], true);
}

#[tokio::test]
async fn blocks_input_with_provider_error_mode() {
    let bundle = PolicyBundle::from_json_str(
        r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"enforce",
        "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
    )
    .unwrap();
    let engine = PolicyEngine::from_bundle(
        &bundle,
        EngineOptions {
            block_mode: noveum_ai_gateway::policy::BlockResponseMode::ProviderError,
            ..Default::default()
        },
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "ssn 123-45-6789"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "noveum_guard_blocked");
}

#[tokio::test]
async fn masks_pii_in_forwarded_request() {
    let engine = engine_from(
        r#"{"policies":[{"name":"pii","type":"pii_detection","mode":"enforce",
        "config":{"phase":"input","entities":["EMAIL_ADDRESS"],"action":"redact"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "email me at jane@acme.io"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    let forwarded = body["messages"][0]["content"].as_str().unwrap();
    assert!(
        !forwarded.contains("jane@acme.io"),
        "email should be redacted: {forwarded}"
    );
    assert!(forwarded.contains("[REDACTED]"));
}

#[tokio::test]
async fn model_allowlist_blocks_disallowed_model() {
    let engine = engine_from(
        r#"{"policies":[{"name":"allow","type":"model_allowlist","mode":"enforce",
        "config":{"allowed":["gpt-4o-mini"],"action":"block"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get("x-noveum-guard-blocked").unwrap(),
        "true"
    );
}

#[tokio::test]
async fn output_phase_blocks_leaked_secret() {
    let engine = engine_from(
        r#"{"policies":[{"name":"secrets","type":"secrets_detection","mode":"enforce",
        "config":{"phase":"output","detectors":["aws_access_key"],"action":"block"}}]}"#,
    );
    let app = router(engine);
    // input is clean; the canned handler returns a response containing an AWS key
    let req = post_json(
        "/v1/canned",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "ok"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get("x-noveum-guard-blocked").unwrap(),
        "true"
    );
}

#[tokio::test]
async fn output_phase_redacts_secret() {
    let engine = engine_from(
        r#"{"policies":[{"name":"secrets","type":"secrets_detection","mode":"enforce",
        "config":{"phase":"output","detectors":["aws_access_key"],"action":"redact"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/canned",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "ok"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(!content.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(content.contains("[REDACTED_SECRET]"));
}

#[tokio::test]
async fn shadow_mode_passes_through_but_does_not_block() {
    let engine = engine_from(
        r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"shadow",
        "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
    );
    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "ssn 123-45-6789"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    // shadow: not blocked, request forwarded unchanged
    assert!(resp.headers().get("x-noveum-guard-blocked").is_none());
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("123-45-6789"));
}

#[tokio::test]
async fn non_json_request_passes_through() {
    let engine = engine_from(
        r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"enforce",
        "config":{"phase":"input","patterns":[{"name":"ssn","regex":"x"}],"action":"block"}}]}"#,
    );
    let app = router(engine);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // non-JSON content type: guard does not inspect, forwards to handler
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn disabled_engine_is_pure_passthrough() {
    let app = router(PolicyEngine::disabled());
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "ssn 123-45-6789"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    let (status, _) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
}

/// NOV-107: a policy the engine cannot compile must not be a silent no-op on the
/// live request path. A `failClosed` one blocks; a fail-open one lets traffic
/// through but is still visible to the operator via `rejected_policies()`.
#[tokio::test]
async fn fail_closed_unenforceable_policy_blocks_the_request_path() {
    // `content_moderation` is in the contract but needs an external classifier
    // this build does not call, so it can never take effect. Marked
    // `failClosed`, the honest outcome is to block rather than to quietly admit
    // everything the operator believed was being moderated.
    let engine = engine_from(
        r#"{"policies":[{"name":"moderation","type":"content_moderation","mode":"enforce",
        "failClosed":true,"config":{"categories":["hate","violence"]}}]}"#,
    );
    assert_eq!(engine.rejected_policy_count(), 1);
    assert!(engine.rejected_policies()[0].contains("content_moderation"));

    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get("x-noveum-guard-blocked").unwrap(),
        "true"
    );
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK); // synthetic_success default
    assert_eq!(body["x_noveum_guard"]["blocked"], true);
}

#[tokio::test]
async fn unknown_policy_type_is_reported_but_fails_open() {
    // A typo'd type must be loudly visible without taking production down.
    let engine = engine_from(
        r#"{"policies":[{"name":"typo","type":"promt_injection","mode":"enforce","config":{}}]}"#,
    );
    assert_eq!(engine.active_policy_count(), 0);
    let rejected = engine.rejected_policies();
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].contains("promt_injection"), "{rejected:?}");

    let app = router(engine);
    let req = post_json(
        "/v1/chat/completions",
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]}),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.headers().get("x-noveum-guard-blocked").is_none());
    let (status, body) = response_json(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["messages"][0]["content"], "hello");
}
