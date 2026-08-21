//! Integration tests for the platform-managed Nova Guard bridge.
//!
//! A `wiremock` server stands in for the Noveum platform API
//! (`/policies/effective`, `/policies/state`, `/policies/usage`). These tests
//! exercise the real HTTP fetch/translate layer, the usage reporter's batching +
//! wire shapes, the ALLOWED telemetry exporter, and the end-to-end guard
//! middleware blocking + BLOCKED usage reporting.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{header, Request, StatusCode},
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{header as match_header, method, path as match_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use noveum_ai_gateway::policy::engine::EngineOptions;
use noveum_ai_gateway::policy::middleware::{guard_middleware, GuardState};
use noveum_ai_gateway::policy::platform::translate_bundle;
use noveum_ai_gateway::policy::remote::{
    bootstrap_engine, fetch_bundle_conditional, fetch_bundle_with_etag, PolicyFetch, RemoteConfig,
    RemoteLiveState,
};
use noveum_ai_gateway::policy::usage::{new_event_id, UsageEvent, UsageReporter};
use noveum_ai_gateway::policy::PolicyEngine;
use noveum_ai_gateway::telemetry::{
    metrics::MetricsExporter, NovaGuardUsagePlugin, RequestMetrics,
};

const PROJECT: &str = "test-proj";

fn cfg(base: &str) -> RemoteConfig {
    RemoteConfig {
        base_url: base.trim_end_matches('/').to_string(),
        api_key: "test-key".to_string(),
        project_id: PROJECT.to_string(),
    }
}

fn eff_path() -> String {
    format!("/api/v1/projects/{PROJECT}/policies/effective")
}
fn state_path() -> String {
    format!("/api/v1/projects/{PROJECT}/policies/state")
}
fn usage_path() -> String {
    format!("/api/v1/projects/{PROJECT}/policies/usage")
}

/// One enabled, mode-less (deprecated-null-mode) COST_CAP over `max_usd`, with a
/// `policyId` so we can assert it propagates into the BLOCKED usage event.
fn cost_cap_payload(max_usd: f64) -> Value {
    json!({"policies":[{
        "policyId":"pol_1","name":"Daily cap","type":"COST_CAP",
        "enabled":true,"failClosed":true,
        "config":{"window":"1d_rolling","maxUsd":max_usd,"action":"BLOCK"}
    }]})
}

/// Stub upstream: echoes the request body back as a 200. Only reached when the
/// guard allows the request.
async fn echo_handler(body: Bytes) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Poll the mock server until at least `min` usage events have been received on
/// `path` (or the timeout elapses), returning every event flattened out of the
/// posted arrays.
async fn wait_for_usage(
    server: &MockServer,
    path: &str,
    min: usize,
    timeout: Duration,
) -> Vec<Value> {
    let start = std::time::Instant::now();
    loop {
        let reqs = server.received_requests().await.unwrap_or_default();
        let mut events = Vec::new();
        for r in reqs.iter().filter(|r| r.url.path() == path) {
            if let Ok(v) = serde_json::from_slice::<Value>(&r.body) {
                match v {
                    Value::Array(a) => events.extend(a),
                    other => events.push(other),
                }
            }
        }
        if events.len() >= min || start.elapsed() > timeout {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Engine options mirroring the platform path: a live-state backend is wired, so
/// `failClosed` is honored (not neutralized).
fn backed_opts() -> EngineOptions {
    EngineOptions {
        live_state_backed: true,
        ..Default::default()
    }
}

/// A 202 stub for `POST /policies/usage`.
async fn mount_usage_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(match_path(usage_path()))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({"success":true,"accepted":1,"persisted":1,"blocked":0})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn effective_fetch_translates_and_honors_etag() {
    let server = MockServer::start().await;
    // First fetch: no If-None-Match → 200 with an ETag + one cost_cap.
    Mock::given(method("GET"))
        .and(match_path(eff_path()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"v1\"")
                .set_body_json(cost_cap_payload(10.0)),
        )
        .mount(&server)
        .await;

    let c = cfg(&server.uri());
    let (bundle, etag) = fetch_bundle_with_etag(&c).await.expect("initial fetch");
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    let engine = PolicyEngine::from_bundle(&bundle, backed_opts());
    assert_eq!(
        engine.active_policy_count(),
        1,
        "effective payload compiles to one active cost_cap"
    );

    // Second fetch carries If-None-Match → the platform answers 304.
    server.reset().await;
    Mock::given(method("GET"))
        .and(match_path(eff_path()))
        .and(match_header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    match fetch_bundle_conditional(&c, etag.as_deref())
        .await
        .expect("conditional fetch")
    {
        PolicyFetch::NotModified => {}
        PolicyFetch::Modified { .. } => panic!("expected 304 NotModified"),
    }
}

#[tokio::test]
async fn usage_reporter_posts_allowed_and_blocked_shapes() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_usage_ok(&server).await;

    let reporter = UsageReporter::spawn(cfg(&server.uri()));
    reporter.report(UsageEvent::allowed(
        new_event_id(),
        "gpt-4o",
        0.0025,
        1000,
        500,
    ));
    reporter.report(UsageEvent::blocked(
        new_event_id(),
        "gpt-4o",
        "COST_CAP",
        Some("pol_1".to_string()),
        Some("daily cap exceeded".to_string()),
    ));

    let events = wait_for_usage(&server, &usage_path(), 2, Duration::from_secs(5)).await;
    let allowed = events
        .iter()
        .find(|e| e.get("outcome").is_none())
        .expect("an ALLOWED event");
    assert_eq!(allowed["costUsd"], 0.0025);
    assert_eq!(allowed["inputTokens"], 1000);
    assert_eq!(allowed["outputTokens"], 500);
    assert_eq!(allowed["requestCount"], 1);

    let blocked = events
        .iter()
        .find(|e| e.get("outcome").map(|o| o == "BLOCKED").unwrap_or(false))
        .expect("a BLOCKED event");
    assert_eq!(blocked["blockedBy"], "COST_CAP");
    assert_eq!(blocked["policyId"], "pol_1");
    assert_eq!(blocked["costUsd"], 0.0);
}

/// A rolling restart must not strand queued usage: `shutdown()` drains whatever
/// the periodic flush never got to.
///
/// Deterministic without touching `NOVEUM_GUARD_USAGE_FLUSH_MS`: `#[tokio::test]`
/// runs on a current-thread runtime and nothing between `spawn` and `shutdown`
/// awaits, so the background flush task is not polled until after `shutdown()`
/// has closed the queue — every event here is delivered by the shutdown drain.
#[tokio::test]
async fn shutdown_flushes_queued_usage_events() {
    let server = MockServer::start().await;
    mount_usage_ok(&server).await;

    let reporter = UsageReporter::spawn(cfg(&server.uri()));
    reporter.report(UsageEvent::allowed(new_event_id(), "gpt-4o", 0.01, 20, 8));
    reporter.report(UsageEvent::blocked(
        new_event_id(),
        "gpt-4o",
        "RATE_LIMIT",
        None,
        None,
    ));
    assert_eq!(reporter.pending_events(), 2, "queued, not yet flushed");

    let outcome = reporter.shutdown(Duration::from_secs(5)).await;

    assert!(!outcome.timed_out, "mock server answers immediately");
    assert_eq!(outcome.delivered, 2, "shutdown drained both events");
    assert_eq!(outcome.failed, 0);
    assert_eq!(outcome.pending, 0);

    let events = wait_for_usage(&server, &usage_path(), 2, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 2, "both events reached the platform");

    // After shutdown the queue is closed: late events are rejected and counted
    // rather than silently accepted into a queue nothing will drain.
    reporter.report(UsageEvent::allowed(new_event_id(), "gpt-4o", 0.5, 1, 1));
    assert_eq!(reporter.pending_events(), 0);
    assert_eq!(reporter.dropped_events(), 1);
}

#[tokio::test]
async fn allowed_exporter_reports_successful_call() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_usage_ok(&server).await;

    let plugin = NovaGuardUsagePlugin::new(UsageReporter::spawn(cfg(&server.uri())));
    plugin
        .export_metrics(RequestMetrics {
            model: "gpt-4o".to_string(),
            status_code: 200,
            cost: Some(0.02),
            input_tokens: Some(30),
            output_tokens: Some(12),
            ..Default::default()
        })
        .await
        .expect("export ok");

    let events = wait_for_usage(&server, &usage_path(), 1, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 1);
    assert!(
        events[0].get("outcome").is_none(),
        "successful call → ALLOWED"
    );
    assert_eq!(events[0]["costUsd"], 0.02);
    assert_eq!(events[0]["model"], "gpt-4o");
}

#[tokio::test]
async fn middleware_blocks_over_cap_and_reports_blocked_event() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    // Live state is over the cap.
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":25.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let bundle = translate_bundle(&cost_cap_payload(10.0)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.headers().contains_key("x-noveum-guard-blocked"),
        "spend over the cap must block the request"
    );

    let events = wait_for_usage(&server, &usage_path(), 1, Duration::from_secs(5)).await;
    let ev = &events[0];
    assert_eq!(ev["outcome"], "BLOCKED");
    assert_eq!(ev["blockedBy"], "COST_CAP");
    assert_eq!(ev["policyId"], "pol_1");
    assert_eq!(ev["model"], "gpt-4o");
}

#[tokio::test]
async fn middleware_allows_under_cap_and_reports_no_block() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    // Live state is well under the cap.
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":1.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let bundle = translate_bundle(&cost_cap_payload(10.0)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !resp.headers().contains_key("x-noveum-guard-blocked"),
        "under the cap the request must pass through"
    );

    // Give any (erroneous) BLOCKED report a chance to land, then assert none did.
    // The guard middleware only emits BLOCKED events; the ALLOWED path is the
    // telemetry exporter, which isn't wired into this bare router.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let events = wait_for_usage(&server, &usage_path(), 1, Duration::from_millis(50)).await;
    assert!(
        events.is_empty(),
        "an allowed request must not emit a BLOCKED usage event, got {events:?}"
    );
}

#[tokio::test]
async fn fail_closed_blocks_when_state_unavailable() {
    let server = MockServer::start().await;
    // /state is down (500) → a failClosed cost_cap must block (fail closed).
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    // failClosed:true is carried through by the translation layer.
    let bundle = translate_bundle(&cost_cap_payload(10.0)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.headers().contains_key("x-noveum-guard-blocked"),
        "failClosed policy must block when /state is unavailable"
    );
}

#[tokio::test]
async fn platform_client_sends_stable_user_agent() {
    // Production api.noveum.ai's edge rejects UA-less requests with 403 before
    // they reach the app — every platform call must carry the gateway UA.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(eff_path()))
        .and(wiremock::matchers::header_regex(
            "user-agent",
            r"^noveum-ai-gateway/\d+\.\d+\.\d+",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(cost_cap_payload(10.0)))
        .expect(1)
        .mount(&server)
        .await;

    let c = cfg(&server.uri());
    let (bundle, _etag) = fetch_bundle_with_etag(&c).await.expect("fetch succeeds");
    assert_eq!(bundle.policies.len(), 1);
    // The mock's `.expect(1)` verifies the UA matched on drop.
}

#[tokio::test]
async fn expired_state_that_cannot_be_revalidated_is_unavailable() {
    // Warm an under-cap snapshot, then take the platform down: once the TTL
    // lapses, get() must report None (unavailable) — NOT serve the stale
    // under-cap snapshot — so failClosed policies block instead of forwarding.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":1.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let c = cfg(&server.uri());
    let live = RemoteLiveState::with_ttl(c, Duration::from_millis(50));
    let warm = live.get().await;
    assert!(warm.is_some(), "warm-up snapshot fetch succeeds");

    tokio::time::sleep(Duration::from_millis(80)).await; // let the TTL lapse
    assert!(
        live.get().await.is_none(),
        "expired snapshot + failing /state must be unavailable, not stale-served"
    );
    // Within the error backoff, callers keep seeing unavailable (fast path).
    assert!(live.get().await.is_none());
}

#[tokio::test]
async fn pending_spend_blocks_concurrent_burst_near_cap() {
    // The platform's counters lag (usage posts async, /state is cached). Two
    // back-to-back requests against an almost-exhausted cap must not BOTH pass:
    // the first request's predicted cost is charged to the in-process pending
    // ledger and the second evaluates against spend + pending.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    // Platform spend stays frozen at 0 (counters lag) with a tiny cap.
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    // Cap of $0.02: one gpt-4o call with max_tokens 1000 predicts ~$0.01.
    let bundle = translate_bundle(&cost_cap_payload(0.02)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body =
        json!({"model":"gpt-4o","max_tokens":1000,"messages":[{"role":"user","content":"hi"}]});
    let mk = || {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-provider-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };
    let first = app.clone().oneshot(mk()).await.unwrap();
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "first request fits under the cap"
    );
    let second = app.oneshot(mk()).await.unwrap();
    assert!(
        second.headers().contains_key("x-noveum-guard-blocked"),
        "second request must see the first one's pending spend and block"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_burst_respects_max_requests_one() {
    // The reviewer's reproduction: maxRequests=1 with frozen zero platform
    // counters and a burst of simultaneous requests. Request/token reservations
    // are folded into the rate counters atomically at admission, so exactly ONE
    // request may pass — not all of them.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{"requests_1m":0},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let platform = json!({"policies":[{
        "policyId":"pol_rl","name":"one per minute","type":"RATE_LIMIT",
        "enabled":true,"failClosed":true,
        "config":{"windows":[{"period":"1m","maxRequests":1,"action":"BLOCK"}]}
    }]});
    let bundle = translate_bundle(&platform).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let futures: Vec<_> = (0..10)
        .map(|_| {
            let app = app.clone();
            let body = serde_json::to_vec(&body).unwrap();
            async move {
                let req = Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer sk-provider-test")
                    .body(Body::from(body))
                    .unwrap();
                let resp = app.oneshot(req).await.unwrap();
                resp.headers().contains_key("x-noveum-guard-blocked")
            }
        })
        .collect();
    let results = futures_util::future::join_all(futures).await;
    let allowed = results.iter().filter(|blocked| !**blocked).count();
    let blocked = results.iter().filter(|blocked| **blocked).count();
    assert_eq!(
        allowed, 1,
        "maxRequests=1 with frozen counters must admit exactly one of the burst"
    );
    assert_eq!(blocked, 9);
}

/// A slow upstream: response headers only arrive after `delay`.
async fn slow_handler(body: Bytes) -> Response {
    tokio::time::sleep(Duration::from_secs(5)).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[tokio::test]
async fn cancellation_before_upstream_headers_does_not_leak_an_active_reservation() {
    // Reviewer's reproduction: the client disconnects while the gateway is
    // still awaiting the provider's response headers. The reservation guard is
    // taken at `reserve()` time, so dropping the middleware future must move the
    // entry out of ACTIVE (into the bounded post-completion TTL) instead of
    // leaving it pinned until the 15-minute leak backstop — which used to keep a
    // `maxRequests: 1` policy 403-blocked long after the client had gone.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{"requests_1m":0},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let platform = json!({"policies":[{
        "policyId":"pol_rl","name":"one per minute","type":"RATE_LIMIT",
        "enabled":true,"failClosed":true,
        "config":{"windows":[{"period":"1m","maxRequests":1,"action":"BLOCK"}]}
    }]});
    let bundle = translate_bundle(&platform).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let pending = Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new());
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: pending.clone(),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(slow_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    // Client hangs up long before the 5s upstream responds: the in-flight
    // middleware future is dropped.
    let cancelled = tokio::time::timeout(Duration::from_millis(500), app.oneshot(req)).await;
    assert!(cancelled.is_err(), "the request must still be in flight");

    // The reservation must have been admitted...
    assert!(
        pending.sum().requests >= 1,
        "the cancelled request should have reserved before forwarding"
    );
    // ...and must NOT still be ACTIVE.
    assert_eq!(
        pending.active_count(),
        0,
        "cancellation before upstream headers leaked an ACTIVE reservation"
    );
}

#[tokio::test]
async fn explicit_include_usage_false_is_overridden_while_metering() {
    // A caller must not be able to select approximate accounting for their own
    // spend: with platform metering active, `include_usage` is forced true even
    // when the request explicitly asks for false.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let bundle = translate_bundle(&cost_cap_payload(10.0)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    // `echo_handler` returns the FORWARDED body, so the response shows exactly
    // what the provider would have received.
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    for stream_options in [
        json!({"include_usage": false}),
        json!({}),
        json!({"include_usage": false, "other": 1}),
        Value::Null,
    ] {
        let mut body = json!({
            "model":"gpt-5.6-luna","stream":true,
            "messages":[{"role":"user","content":"hi"}]
        });
        if !stream_options.is_null() {
            body["stream_options"] = stream_options.clone();
        }
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-provider", "openai")
            .header("authorization", "Bearer sk-provider-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let forwarded: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            forwarded["stream_options"]["include_usage"],
            json!(true),
            "include_usage must be forced true (sent: {stream_options})"
        );
        // Unrelated keys in `stream_options` survive.
        if stream_options.get("other").is_some() {
            assert_eq!(forwarded["stream_options"]["other"], json!(1));
        }
    }
}

#[tokio::test]
async fn overflow_sized_max_tokens_is_rejected_with_400() {
    // `max_tokens` is untrusted request JSON: `u64::MAX` used to panic the
    // request task in debug builds (client got an empty response) and wrap the
    // token reservation in release builds. It must be a deterministic 400.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{},"stale":false,"ttlSeconds":30}),
        ))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;

    let c = cfg(&server.uri());
    let bundle = translate_bundle(&cost_cap_payload(10.0)).unwrap();
    let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
    let pending = Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new());
    let gs = GuardState {
        engine,
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: pending.clone(),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    for (field, value) in [
        ("max_tokens", json!(u64::MAX)),
        ("max_completion_tokens", json!(u64::MAX)),
        ("max_output_tokens", json!(u64::MAX)),
        ("max_tokens", json!(-1)),
        ("max_tokens", json!(0)),
        ("max_tokens", json!(10_000_001u64)),
    ] {
        let mut body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
        body[field] = value.clone();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-provider-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{field}={value} must be a deterministic client error"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let err: Value = serde_json::from_slice(&bytes).expect("structured JSON error");
        assert_eq!(err["error"]["type"], "invalid_request_error");
        assert_eq!(err["error"]["param"], field);
    }

    // A rejected request must not leave a reservation behind.
    assert_eq!(pending.active_count(), 0);

    // ...and a sane limit still passes through untouched.
    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// §4.5 — an explicitly configured guard must never degrade to pass-through.
// ---------------------------------------------------------------------------

/// A configured platform bridge whose FIRST policy fetch fails must not start.
/// There is no known policy set at that point, so every request would be
/// forwarded with zero enforcement while the deployment looks healthy.
#[tokio::test]
async fn initial_policy_fetch_failure_does_not_serve_unguarded_traffic() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(format!(
            "/api/v1/projects/{PROJECT}/policies/effective"
        )))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&server)
        .await;

    let opts = EngineOptions {
        live_state_backed: true,
        ..Default::default()
    };
    let Err(err) = bootstrap_engine(&cfg(&server.uri()), opts.clone(), false).await else {
        panic!("a failed cold-start fetch must abort startup");
    };
    // The message has to tell an operator what happened and how to override.
    assert!(err.contains("unguarded"), "unhelpful message: {err}");
    assert!(
        err.contains(noveum_ai_gateway::policy::remote::ALLOW_UNGUARDED_START_VAR),
        "message must name the override var: {err}"
    );

    // The escape hatch exists, but it is opt-in and yields an EMPTY engine —
    // never a silently-loaded local bundle standing in for platform policy.
    let (engine, etag) = bootstrap_engine(&cfg(&server.uri()), opts, true)
        .await
        .expect("the override must allow an unguarded start");
    assert_eq!(engine.active_policy_count(), 0);
    assert!(etag.is_none());
}

/// The happy path still compiles the fetched bundle and returns its ETag for
/// the poller to revalidate against.
#[tokio::test]
async fn successful_cold_start_compiles_platform_policies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(format!(
            "/api/v1/projects/{PROJECT}/policies/effective"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_json(json!({
                    "policies": [{
                        "id": "p1",
                        "name": "cap",
                        "type": "COST_CAP",
                        "enabled": true,
                        "source": "project",
                        "config": { "window": "30d_rolling", "maxUsd": 100.0, "action": "BLOCK" }
                    }]
                })),
        )
        .mount(&server)
        .await;

    let (engine, etag) = bootstrap_engine(
        &cfg(&server.uri()),
        EngineOptions {
            live_state_backed: true,
            ..Default::default()
        },
        false,
    )
    .await
    .expect("a successful fetch must start");
    assert_eq!(engine.active_policy_count(), 1);
    assert_eq!(etag.as_deref(), Some("\"v1\""));
}

/// A *configured* local bundle that cannot be parsed is a configuration error,
/// not a pass-through. An *absent* one stays pass-through: that is the default
/// deployment and must keep booting.
#[tokio::test]
async fn configured_but_malformed_local_bundle_is_an_error() {
    use noveum_ai_gateway::policy::source::{load_from_env, load_from_file};

    let dir = std::env::temp_dir();

    // Malformed file → Err, which `PolicyEngine::from_env` now propagates
    // instead of swallowing into an empty engine.
    let bad = dir.join(format!("nova-guard-bad-{}.json", uuid::Uuid::new_v4()));
    tokio::fs::write(&bad, "{ \"policies\": [ ").await.unwrap();
    let e = load_from_file(bad.to_str().unwrap())
        .await
        .expect_err("malformed bundle must not parse");
    assert!(e.contains("not a valid nova-guard bundle"), "got: {e}");
    tokio::fs::remove_file(&bad).await.ok();

    // A syntactically valid but EMPTY bundle is legitimate: it means "no
    // policies", and must load cleanly rather than being mistaken for broken.
    let empty = dir.join(format!("nova-guard-empty-{}.json", uuid::Uuid::new_v4()));
    tokio::fs::write(&empty, r#"{"policies":[]}"#)
        .await
        .unwrap();
    let bundle = load_from_file(empty.to_str().unwrap())
        .await
        .expect("an empty bundle is valid configuration");
    assert_eq!(bundle.policies.len(), 0);
    tokio::fs::remove_file(&empty).await.ok();

    // No source configured at all → empty bundle, no error. (This test process
    // sets neither `NOVEUM_GUARD_POLICIES_FILE` nor `NOVEUM_GUARD_POLICIES`.)
    let none = load_from_env()
        .await
        .expect("an unconfigured source is not an error");
    assert_eq!(none.policies.len(), 0);
}

/// The backend now answers a total Redis+Postgres outage with
/// `503 GUARDRAIL_STATE_UNAVAILABLE` instead of a 200 carrying zeroed counters.
/// Both gateway dispositions must handle that explicitly: fail-closed blocks,
/// fail-open allows but records *why*. Neither may read the 503 as "$0 spent".
#[tokio::test]
async fn state_503_is_unavailable_state_not_zero_usage() {
    for fail_closed in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path(state_path()))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "error": { "code": "GUARDRAIL_STATE_UNAVAILABLE" }
            })))
            .mount(&server)
            .await;
        mount_usage_ok(&server).await;

        let c = cfg(&server.uri());
        // A cap of $10 that zeroed counters would sail straight through.
        let payload = json!({"policies":[{
            "policyId":"pol_1","name":"Daily cap","type":"COST_CAP",
            "enabled":true,"failClosed":fail_closed,
            "config":{"window":"1d_rolling","maxUsd":10.0,"action":"BLOCK"}
        }]});
        let bundle = translate_bundle(&payload).unwrap();
        let engine = Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts()));
        let gs = GuardState {
            engine,
            live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
            usage: Some(UsageReporter::spawn(c.clone())),
            pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
            admission: None,
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(echo_handler))
            .layer(from_fn_with_state(gs, guard_middleware));

        let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer sk-provider-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        if fail_closed {
            assert!(
                resp.headers().contains_key("x-noveum-guard-blocked"),
                "failClosed policy must block on a 503 /state"
            );
        } else {
            assert!(
                !resp.headers().contains_key("x-noveum-guard-blocked"),
                "fail-open policy must allow on a 503 /state"
            );
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }
}

/// A 503 must not be parsed as a state document. The live-state client has to
/// surface "unavailable" so the engine can apply its fail-closed/fail-open
/// semantics — silently yielding an all-zero `LiveState` would satisfy every
/// cost cap during an outage.
#[tokio::test]
async fn state_503_never_yields_a_zero_valued_live_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": { "code": "GUARDRAIL_STATE_UNAVAILABLE" }
        })))
        .mount(&server)
        .await;

    let live = RemoteLiveState::new(cfg(&server.uri()));
    let state = live.get().await;
    assert!(
        state.is_none(),
        "a 503 must read as unavailable state, not as zeroed counters"
    );
}

// ---------------------------------------------------------------------------
// §6.1 — cross-replica atomic admission (NOV-119).
//
// The per-process `PendingSpend` ledger cannot bound spend across replicas: with
// N pods a $100 cap is enforced N times over. In STRICT enforcement mode the
// gateway asks the platform's atomic admission API instead, holds the returned
// reservation for the life of the request, and settles it
// (complete/abandon/cancel) on the way out.
// ---------------------------------------------------------------------------

use noveum_ai_gateway::policy::admission::AdmissionClient;
use noveum_ai_gateway::policy::PolicyBundle;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn admit_path() -> String {
    format!("/api/v1/projects/{PROJECT}/policies/admit")
}

/// A COST_CAP in **strict** enforcement mode, in the platform's own casing
/// (`STRICT`) so the translation layer's normalization is exercised too.
fn strict_cost_cap_payload(max_usd: f64, fail_closed: bool) -> Value {
    json!({"policies":[{
        "policyId":"pol_strict","name":"Org cap","type":"COST_CAP",
        "enabled":true,"failClosed":fail_closed,
        "config":{"window":"1d_rolling","maxUsd":max_usd,"action":"BLOCK",
                  "enforcementMode":"STRICT"}
    }]})
}

/// `/state` reporting no spend: the strict path still fetches it (rate limits
/// and advisory caps share it), and it must not be what blocks these tests.
async fn mount_state_zero(server: &MockServer) {
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"cost":{"1d_rolling":0.0},"rate":{"requests_1m":0},"stale":false,"ttlSeconds":30}),
        ))
        .mount(server)
        .await;
}

/// `POST /admit` → a fixed allow carrying `reservationId`.
async fn mount_admit_allowed(server: &MockServer, reservation_id: &str) {
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allowed": true, "reservationId": reservation_id,
            "expiresAt": "2099-01-01T00:00:00Z", "policyVersion": "\"v1\"",
            "replayed": false, "shadowed": []
        })))
        .mount(server)
        .await;
}

/// A catch-all `200` for the three settlement endpoints.
async fn mount_settlement_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(format!(
            r"^/api/v1/projects/{PROJECT}/policies/reservations/[^/]+/(complete|abandon|cancel)$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "status": "completed", "reservationAbsent": false,
            "applied": true, "retained": false, "replayed": false
        })))
        .mount(server)
        .await;
}

/// Wait until at least `min` requests whose path ends with `suffix` have been
/// received, returning `(path, parsed body)` for each.
async fn wait_for_path_suffix(
    server: &MockServer,
    suffix: &str,
    min: usize,
    timeout: Duration,
) -> Vec<(String, Value)> {
    let start = std::time::Instant::now();
    loop {
        let reqs = server.received_requests().await.unwrap_or_default();
        let hits: Vec<(String, Value)> = reqs
            .iter()
            .filter(|r| r.url.path().ends_with(suffix))
            .map(|r| {
                (
                    r.url.path().to_string(),
                    serde_json::from_slice(&r.body).unwrap_or(Value::Null),
                )
            })
            .collect();
        if hits.len() >= min || start.elapsed() > timeout {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A guard state wired for strict admission against `server`.
fn strict_guard_state(server_uri: &str, bundle: &PolicyBundle) -> GuardState {
    let c = cfg(server_uri);
    GuardState {
        engine: Arc::new(PolicyEngine::from_bundle(bundle, backed_opts())),
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c.clone())),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: Some(Arc::new(AdmissionClient::new(c, None))),
    }
}

fn chat_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-test")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// Upstream that answers with a provider-shaped body carrying real token counts.
async fn usage_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json!({
            "choices":[{"message":{"role":"assistant","content":"pong"}}],
            "usage":{"prompt_tokens":123,"completion_tokens":45,"total_tokens":168}
        })
        .to_string(),
    )
        .into_response()
}

/// Upstream that answers with no `usage` block at all (the shape that must
/// abandon rather than complete with invented numbers).
async fn no_usage_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"choices":[{"message":{"role":"assistant","content":"pong"}}]}).to_string(),
    )
        .into_response()
}

/// Upstream that answers with an SSE stream. The guard passes streams through
/// without output enforcement, so no authoritative usage is recoverable in that
/// layer and the reservation settles via `abandon`.
async fn sse_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n",
    )
        .into_response()
}

#[tokio::test]
async fn strict_admission_reserves_then_completes_with_real_usage() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-abc").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drain the body so the RAII guard reaches its terminal state.
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();

    // The admit call carried this request's estimate, in the platform's shape.
    let admits = wait_for_path_suffix(&server, "/admit", 1, Duration::from_secs(5)).await;
    assert_eq!(admits.len(), 1, "exactly one admission per request");
    let a = &admits[0].1;
    assert_eq!(a["model"], "gpt-4o");
    assert_eq!(a["provider"], "openai");
    assert_eq!(a["maximumOutputTokens"], 256);
    assert!(a["requestId"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(a["estimatedInputTokens"].as_u64().is_some());
    assert!(a["estimatedCostUsd"].as_f64().unwrap() > 0.0);

    // ...and settlement carried the PROVIDER's counts, not the estimate.
    let completes = wait_for_path_suffix(&server, "/complete", 1, Duration::from_secs(5)).await;
    assert_eq!(
        completes.len(),
        1,
        "one completion, on the right reservation"
    );
    assert!(completes[0].0.contains("/reservations/res-abc/"));
    let c = &completes[0].1;
    assert_eq!(c["inputTokens"], 123);
    assert_eq!(c["outputTokens"], 45);
    assert_eq!(c["requestCount"], 1);
    assert_eq!(c["model"], "gpt-4o");
    assert!(c["costUsd"].as_f64().unwrap() > 0.0);
    // Nothing was abandoned or cancelled for a normal completion.
    assert!(
        wait_for_path_suffix(&server, "/abandon", 1, Duration::from_millis(50))
            .await
            .is_empty()
    );
    assert!(
        wait_for_path_suffix(&server, "/cancel", 1, Duration::from_millis(50))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn strict_admission_includes_declared_anthropic_cache_write_premium() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-cache-premium").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    for cache_control in [Value::Null, json!({"type": "ephemeral", "ttl": "1h"})] {
        let mut body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 100,
            "inference_geo": "global",
            "messages": [{"role": "user", "content": "cache me"}]
        });
        if !cache_control.is_null() {
            body["cache_control"] = cache_control;
        }
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-provider", "anthropic")
            .header("authorization", "Bearer sk-ant-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
    }

    let admits = wait_for_path_suffix(&server, "/admit", 2, Duration::from_secs(5)).await;
    assert_eq!(admits.len(), 2);
    let plain = admits[0].1["estimatedCostUsd"].as_f64().unwrap();
    let cached = admits[1].1["estimatedCostUsd"].as_f64().unwrap();
    let input_tokens = admits[1].1["estimatedInputTokens"].as_u64().unwrap() as f64;
    let expected_cached = (input_tokens / 1_000_000.0) * 4.0 + (100.0 / 1_000_000.0) * 10.0;
    assert!((cached - expected_cached).abs() < 1e-12);
    assert!(
        cached > plain,
        "1h cache miss must reserve its 2x input rate"
    );
}

#[tokio::test]
async fn strict_admission_counts_the_post_transform_full_json_body() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-full-input").await;
    mount_settlement_ok(&server).await;

    let bundle = PolicyBundle::from_json_str(&format!(
        r#"{{"policies":[
          {{"name":"strict","type":"cost_cap","mode":"enforce","config":{{
            "window":"30d_rolling","maxUsd":100.0,"action":"block","enforcementMode":"strict"
          }}}},
          {{"name":"expand","type":"regex_match","mode":"enforce","config":{{
            "phase":"input","patterns":[{{"name":"x","regex":"x"}}],
            "action":"redact","redactWith":"{}"
          }}}}
        ]}}"#,
        "Y".repeat(8_192)
    ))
    .unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body = json!({
        "model": "gpt-4o",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "x"}],
        "tools": [{"type": "function", "function": {
            "name": "lookup",
            "description": "z".repeat(8_000),
            "parameters": {"type": "object"}
        }}]
    });
    let response = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let forwarded: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        forwarded["messages"][0]["content"].as_str().unwrap().len(),
        8_192,
        "the measured body must be the same expanded body forwarded upstream"
    );

    let admits = wait_for_path_suffix(&server, "/admit", 1, Duration::from_secs(5)).await;
    let estimated = admits[0].1["estimatedInputTokens"].as_u64().unwrap();
    let serialized = serde_json::to_vec(&forwarded).unwrap().len() as u64;
    assert!(
        estimated >= serialized + 4_096,
        "strict admission reserved {estimated} tokens for a {serialized}-byte transformed body with tools"
    );
}

#[tokio::test]
async fn native_anthropic_preflight_rejects_mcp_and_server_tools_before_platform_or_provider() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;

    // Advisory is deliberate: strict admission already rejects these shapes.
    // The shared Anthropic converter must reject them for every stateful policy
    // mode, before either native middleware or the Worker can create a hold or
    // call the provider.
    let bundle = translate_bundle(&json!({"policies":[{
        "policyId":"pol_advisory","name":"Advisory cap","type":"COST_CAP",
        "enabled":true,"failClosed":false,
        "config":{"window":"1d_rolling","maxUsd":100.0,"action":"BLOCK",
                  "enforcementMode":"ADVISORY"}
    }]}))
    .unwrap();
    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let cases = [
        (
            "mcp_servers",
            "mcp_servers",
            json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "mcp_servers": [{"type": "url", "url": "https://mcp.example"}]
            }),
        ),
        (
            "server tool",
            "web_search_20250305",
            json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "tools": [{
                    "type": "web_search_20250305",
                    "name": "web_search",
                    "input_schema": {"type": "object"}
                }]
            }),
        ),
    ];

    for (label, expected_error, body) in cases {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-provider", "anthropic")
            .header("authorization", "Bearer sk-ant-test")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
        let response_body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&response_body).contains(expected_error),
            "the client error must identify {label}"
        );
    }

    assert_eq!(
        upstream_hits.load(Ordering::SeqCst),
        0,
        "unsupported Anthropic tools must never reach the provider"
    );
    assert!(
        wait_for_path_suffix(&server, "/state", 1, Duration::from_millis(100))
            .await
            .is_empty(),
        "provider preflight must run before even the platform state read"
    );
    assert!(
        wait_for_path_suffix(&server, "/admit", 1, Duration::from_millis(100))
            .await
            .is_empty(),
        "provider preflight must run before admission"
    );
}

#[tokio::test]
async fn strict_cap_rejects_opaque_json_and_non_json_before_admission() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "must-not-be-created").await;

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let response_hits = upstream_hits.clone();
    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post({
                let hits = hits.clone();
                move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        usage_handler().await
                    }
                }
            }),
        )
        .route(
            "/v1/responses",
            post(move || {
                let hits = response_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let base = || {
        json!({
            "model": "gpt-4o",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "bounded text"}]
        })
    };
    for (field, value) in [
        ("web_search_options", json!({"search_context_size": "high"})),
        (
            "mcp_servers",
            json!([{"type": "url", "url": "https://mcp.example"}]),
        ),
        ("service_tier", json!("priority")),
    ] {
        let mut opaque = base();
        opaque[field] = value;
        let response = app.clone().oneshot(chat_request(&opaque)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
    }

    let malformed = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from("{not-json"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(malformed).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    let multipart = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "multipart/form-data; boundary=test")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from("--test\r\nopaque\r\n--test--\r\n"))
        .unwrap();
    let response = app.clone().oneshot(multipart).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let responses = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-provider-test")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "max_output_tokens": 64,
                "input": "server-side state may expand this"
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(responses).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    for provider in ["perplexity", "openrouter"] {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("x-provider", provider)
            .header("authorization", "Bearer sk-provider-test")
            .body(Body::from(serde_json::to_vec(&base()).unwrap()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST,
            "{provider}"
        );
    }

    assert_eq!(upstream_hits.load(Ordering::SeqCst), 0);
    assert!(
        wait_for_path_suffix(&server, "/admit", 1, Duration::from_millis(100))
            .await
            .is_empty(),
        "invalid strict input must not create a reservation"
    );
}

/// A strict cost cap is only a hard cap when the reservation covers every
/// token the provider can generate.  Reserving the 1,024-token fallback for an
/// otherwise unbounded request leaves the provider free to emit far more and
/// overshoot the cap, so strict admission must refuse such a request before it
/// creates a reservation or reaches the provider. (The cached `/state` read may
/// already have occurred; it is read-only.)
#[tokio::test]
async fn strict_cost_cap_requires_an_explicit_output_limit() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "must-not-be-created").await;
    mount_settlement_ok(&server).await;

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        !resp.headers().contains_key("x-noveum-guard-blocked"),
        "a malformed client request is not a policy-limit decision"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["code"], "missing_output_limit");
    assert!(error["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("strict Nova Guard cost cap")));

    assert_eq!(upstream_hits.load(Ordering::SeqCst), 0);
    assert!(
        wait_for_path_suffix(&server, "/admit", 1, Duration::from_millis(100))
            .await
            .is_empty(),
        "an unbounded request must be rejected before reserving"
    );
}

#[tokio::test]
async fn deterministic_provider_errors_are_rejected_before_admission_or_provider() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "must-not-be-created").await;
    mount_settlement_ok(&server).await;

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let invalid_body = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 32,
        "n": 2,
        "messages": [{"role":"user","content":"hi"}]
    });
    let invalid_shape = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", "anthropic")
        .header("authorization", "Bearer sk-ant-test")
        .body(Body::from(serde_json::to_vec(&invalid_body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(invalid_shape).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let invalid_auth = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", "anthropic")
        .header("authorization", "Basic c2VjcmV0")
        .body(Body::from(
            json!({
                "model": "claude-sonnet-5",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}]
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(invalid_auth).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let missing_openai_auth = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(missing_openai_auth).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let unsupported_provider = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-test")
        .header("x-provider", "not-a-provider")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(unsupported_provider).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let missing_bedrock_auth = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", "bedrock")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "amazon.nova-micro-v1:0",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(missing_bedrock_auth).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let missing_model = json!({
        "max_tokens": 32,
        "messages": [{"role":"user","content":"hi"}]
    });
    let response = app
        .clone()
        .oneshot(chat_request(&missing_model))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let region_priced_nova = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", "bedrock")
        .header("x-aws-access-key-id", "AKIATEST")
        .header("x-aws-secret-access-key", "secret")
        .header("x-aws-region", "eu-south-1")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "amazon.nova-pro-v1:0",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(region_priced_nova).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["code"], "unsupported_strict_input");
    assert!(error["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("source-region-aware pricing")));

    let native_bedrock = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", "bedrock")
        .header("x-aws-access-key-id", "AKIATEST")
        .header("x-aws-secret-access-key", "secret")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "amazon.nova-micro-v1:0",
                "max_tokens": 32,
                "messages": [{"role":"user","content":"hi"}],
                "inferenceConfig": {"maxTokens": 100000}
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(native_bedrock).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    assert_eq!(upstream_hits.load(Ordering::SeqCst), 0);
    assert!(
        wait_for_path_suffix(&server, "/admit", 1, Duration::from_millis(100))
            .await
            .is_empty(),
        "provider-local validation errors must not create a reservation"
    );
}

#[tokio::test]
async fn platform_block_is_provider_shaped_and_never_reaches_upstream() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;
    // The contract's sharp edge: a block is HTTP 200 with `allowed: false`.
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allowed": false,
            "decision": {
                "policyId":"pol_strict","policyName":"Org cap","policyType":"COST_CAP",
                "scope":"org","dimension":"7d_rolling","limit":1200,"observed":1199.5,
                "projected":1200.5,"reason":"org 7d rolling cap would be exceeded"
            }
        })))
        .mount(&server)
        .await;

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert!(
        resp.headers().contains_key("x-noveum-guard-blocked"),
        "a platform admission block must surface as a Nova Guard block"
    );
    assert_eq!(
        upstream_hits.load(Ordering::SeqCst),
        0,
        "a blocked request must never be dispatched upstream"
    );
    // The platform's reason reaches the client's synthetic response.
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("org 7d rolling cap would be exceeded"),
        "block body should carry the platform's reason: {text}"
    );
    // Nothing was reserved, so nothing may be settled.
    assert!(
        wait_for_path_suffix(&server, "/cancel", 1, Duration::from_millis(200))
            .await
            .is_empty(),
        "a request that was never admitted has no reservation to cancel"
    );
}

#[tokio::test]
async fn admission_503_fails_closed_and_fails_open_per_policy() {
    for fail_closed in [true, false] {
        let server = MockServer::start().await;
        mount_state_zero(&server).await;
        mount_usage_ok(&server).await;
        mount_settlement_ok(&server).await;
        Mock::given(method("POST"))
            .and(match_path(admit_path()))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "message": "GUARDRAIL_ADMISSION_UNAVAILABLE"
            })))
            .mount(&server)
            .await;

        let bundle = translate_bundle(&strict_cost_cap_payload(100.0, fail_closed)).unwrap();
        let app = Router::new()
            .route("/v1/chat/completions", post(usage_handler))
            .layer(from_fn_with_state(
                strict_guard_state(&server.uri(), &bundle),
                guard_middleware,
            ));

        let body =
            json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
        let resp = app.oneshot(chat_request(&body)).await.unwrap();
        if fail_closed {
            assert!(
                resp.headers().contains_key("x-noveum-guard-blocked"),
                "an unevaluable admission must block a failClosed cap — never allow"
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "a fail-open cap keeps serving when admission is unavailable"
            );
            assert!(!resp.headers().contains_key("x-noveum-guard-blocked"));
        }
    }
}

#[tokio::test]
async fn a_stream_that_ends_without_usage_abandons_rather_than_completing() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-nousage").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(no_usage_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();

    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(
        abandons.len(),
        1,
        "no authoritative usage → abandon (estimate stays applied)"
    );
    assert!(abandons[0].0.contains("/reservations/res-nousage/"));
    assert!(abandons[0].1["reason"].as_str().is_some());
    assert!(
        wait_for_path_suffix(&server, "/complete", 1, Duration::from_millis(50))
            .await
            .is_empty(),
        "the gateway must not invent token counts it never saw"
    );
}

#[tokio::test]
async fn a_gateway_policy_block_after_admission_cancels_the_reservation() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-cancelme").await;
    mount_settlement_ok(&server).await;

    // A strict cap (so admission runs) plus a text rule that blocks *after* the
    // reservation is held. The call provably never reaches the provider, which
    // is the one case where releasing the hold is correct.
    let bundle = PolicyBundle::from_json_str(
        r#"{"policies":[
            {"name":"cap","type":"cost_cap","mode":"enforce","failClosed":true,"priority":1,
             "config":{"window":"1d_rolling","maxUsd":100.0,"action":"block",
                       "enforcementMode":"strict"}},
            {"name":"ban","type":"regex_match","mode":"enforce","priority":2,
             "config":{"phase":"input","patterns":[{"name":"secretword","regex":"launchcodes"}],
                       "action":"block"}}
        ]}"#,
    )
    .unwrap();

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits = upstream_hits.clone();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    usage_handler().await
                }
            }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body = json!({"model":"gpt-4o","max_tokens":256,
        "messages":[{"role":"user","content":"the launchcodes are"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert!(resp.headers().contains_key("x-noveum-guard-blocked"));
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 0);

    let cancels = wait_for_path_suffix(&server, "/cancel", 1, Duration::from_secs(5)).await;
    assert_eq!(cancels.len(), 1, "the held reservation must be released");
    assert!(cancels[0].0.contains("/reservations/res-cancelme/"));
    assert!(
        wait_for_path_suffix(&server, "/abandon", 1, Duration::from_millis(50))
            .await
            .is_empty(),
        "abandon would keep charging for a call that never happened"
    );
}

#[tokio::test]
async fn a_retried_admit_reuses_the_same_request_id() {
    // A transient 502 from the edge is retried. Because the SAME `requestId` is
    // replayed, the platform returns the SAME reservation instead of reserving
    // twice — which is what makes retrying safe at all.
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allowed": true, "reservationId": "res-replayed", "replayed": true
        })))
        .mount(&server)
        .await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "the retry must succeed");
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();

    let admits = wait_for_path_suffix(&server, "/admit", 2, Duration::from_secs(5)).await;
    assert_eq!(admits.len(), 2, "the 502 must have been retried");
    let ids: Vec<&str> = admits
        .iter()
        .map(|(_, b)| b["requestId"].as_str().unwrap_or(""))
        .collect();
    assert!(!ids[0].is_empty());
    assert_eq!(
        ids[0], ids[1],
        "a retry MUST reuse the idempotency key, or it double-reserves"
    );
}

#[tokio::test]
async fn client_cancellation_mid_request_still_settles_the_reservation() {
    // The reviewer's cancellation shape, now against the platform: the client
    // disappears while the gateway is still awaiting upstream headers. The
    // reservation must not be left dangling until its server-side expiry.
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-cancelled-client").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(slow_handler))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let cancelled =
        tokio::time::timeout(Duration::from_millis(600), app.oneshot(chat_request(&body))).await;
    assert!(cancelled.is_err(), "the request must still be in flight");

    // ABANDON, not cancel: the request may well have reached the provider, so
    // the conservative estimate has to stay applied.
    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(
        abandons.len(),
        1,
        "a mid-flight cancellation must still settle the reservation"
    );
    assert!(abandons[0]
        .0
        .contains("/reservations/res-cancelled-client/"));
}

// ---------------------------------------------------------------------------
// §6.2 — ONE request must produce exactly ONE metered record.
//
// Two independent paths report a strict-mode request to the platform:
//   * the reservation lifecycle (`/reservations/{id}/complete|abandon`), and
//   * the legacy telemetry exporter (`POST /policies/usage`).
// Each mints its own `eventId`, so neither the Redis dedup marker nor the
// `(organizationId, eventId)` unique index collapses them: metered spend
// doubles and every cost cap effectively halves.
// ---------------------------------------------------------------------------

/// The production wiring, in one place: `main.rs` registers the ALLOWED usage
/// exporter *alongside* the guard middleware, so for a proxied call BOTH run.
/// These tests drive the exporter explicitly because the bare test router has no
/// telemetry layer.
fn usage_plugin(gs: &GuardState) -> NovaGuardUsagePlugin {
    let reporter = gs.usage.clone().expect("platform bridge configured");
    let plugin = NovaGuardUsagePlugin::new(reporter);
    match &gs.admission {
        Some(admission) => plugin.metered_by_admission(gs.engine.clone(), admission.clone()),
        None => plugin,
    }
}

/// What `telemetry::middleware` builds for a completed proxy call.
///
/// `model` is the **provider-resolved** id read back out of the response body
/// (e.g. `claude-haiku-4-5-20251001`), which is why the duplicate rows differ in
/// model spelling from the settlement's — that one carries the id the *caller*
/// asked for (`claude-haiku-4-5`).
fn telemetry_metrics(model: &str, input: u32, output: u32, cost: f64) -> RequestMetrics {
    RequestMetrics {
        provider: "openai".to_string(),
        model: model.to_string(),
        status_code: 200,
        cost: Some(cost),
        input_tokens: Some(input),
        output_tokens: Some(output),
        ..Default::default()
    }
}

/// ALLOWED events that reached `POST /policies/usage` (i.e. legacy metering).
async fn allowed_usage_events(server: &MockServer, timeout: Duration) -> Vec<Value> {
    wait_for_usage(server, &usage_path(), 1, timeout)
        .await
        .into_iter()
        .filter(|e| e.get("outcome").is_none())
        .collect()
}

/// **The reproduction.** A strict-mode request that is admitted, served, and
/// settled through `complete` must be metered exactly ONCE. The settled
/// reservation is the authoritative record, so the legacy `/usage` report has to
/// be suppressed — otherwise the platform records two ALLOWED events for one
/// call and the cap is enforced at half its configured value.
#[tokio::test]
async fn a_settled_strict_request_is_metered_exactly_once() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-once").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let gs = strict_guard_state(&server.uri(), &bundle);
    let plugin = usage_plugin(&gs);
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Drain the body so the RAII guard settles.
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();

    // ...and the telemetry exporter fires for the very same call, with the
    // provider-resolved model id and the provider's own token counts.
    plugin
        .export_metrics(telemetry_metrics("gpt-4o-2024-11-20", 123, 45, 0.000107))
        .await
        .expect("export ok");

    // Record #1: the settlement. This one is authoritative and must happen.
    let completes = wait_for_path_suffix(&server, "/complete", 1, Duration::from_secs(5)).await;
    assert_eq!(completes.len(), 1, "the reservation must be settled");
    assert!(completes[0].0.contains("/reservations/res-once/"));
    assert_eq!(completes[0].1["inputTokens"], 123);

    // Record #2 must NOT exist.
    let duplicates = allowed_usage_events(&server, Duration::from_millis(600)).await;
    assert!(
        duplicates.is_empty(),
        "a settled reservation is the authoritative record; the legacy /usage report \
         double-meters this call, got {duplicates:?}"
    );
}

/// Advisory mode has no reservation lifecycle at all, so the legacy exporter is
/// the ONLY thing that advances the platform's counters. Suppressing it there
/// would not fix double-metering, it would switch metering off.
#[tokio::test]
async fn an_advisory_request_still_meters_through_the_legacy_exporter() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;

    // Same platform bridge, same admission client — only the enforcement mode
    // differs (an advisory cap, i.e. the in-process pending ledger).
    let bundle = translate_bundle(&cost_cap_payload(100.0)).unwrap();
    let gs = strict_guard_state(&server.uri(), &bundle);
    let plugin = usage_plugin(&gs);
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    plugin
        .export_metrics(telemetry_metrics("gpt-4o-2024-11-20", 123, 45, 0.000107))
        .await
        .expect("export ok");

    let events = allowed_usage_events(&server, Duration::from_secs(5)).await;
    assert_eq!(
        events.len(),
        1,
        "advisory mode meters through /usage and nothing else, got {events:?}"
    );
    assert_eq!(events[0]["costUsd"], 0.000107);
    assert_eq!(events[0]["inputTokens"], 123);
    // Advisory never reserves, so nothing is ever admitted or settled.
    assert!(
        wait_for_path_suffix(&server, "/admit", 1, Duration::from_millis(50))
            .await
            .is_empty()
    );
}

/// A gateway with no platform bridge at all keeps the exporter ungated.
#[tokio::test]
async fn a_gateway_without_an_admission_client_reports_unchanged() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_usage_ok(&server).await;

    let plugin = NovaGuardUsagePlugin::new(UsageReporter::spawn(cfg(&server.uri())));
    plugin
        .export_metrics(telemetry_metrics("gpt-4o", 30, 12, 0.02))
        .await
        .expect("export ok");

    let events = allowed_usage_events(&server, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["costUsd"], 0.02);
}

/// Strict mode, admission unavailable, fail-*open* policy: the call is served
/// with no reservation behind it. Nothing platform-side would ever hear about it
/// — so the guard middleware meters it itself, exactly once, and the (silenced)
/// exporter must not add a second record.
#[tokio::test]
async fn a_fail_open_request_that_was_never_reserved_is_still_metered_once() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"message":"GUARDRAIL_ADMISSION_UNAVAILABLE"})),
        )
        .mount(&server)
        .await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, false)).unwrap();
    let gs = strict_guard_state(&server.uri(), &bundle);
    let plugin = usage_plugin(&gs);
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a fail-open cap keeps serving when admission is unavailable"
    );
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    plugin
        .export_metrics(telemetry_metrics("gpt-4o-2024-11-20", 123, 45, 0.000107))
        .await
        .expect("export ok");

    let events = allowed_usage_events(&server, Duration::from_secs(5)).await;
    assert_eq!(
        events.len(),
        1,
        "an unreserved call must be metered exactly once, got {events:?}"
    );
    // Metered with the PROVIDER's counts, not the estimate: the response body
    // carried them.
    assert_eq!(events[0]["inputTokens"], 123);
    assert_eq!(events[0]["outputTokens"], 45);
    assert!(events[0]["costUsd"].as_f64().unwrap() > 0.0);
    // There was no reservation, so there is nothing to settle.
    for endpoint in ["/complete", "/abandon", "/cancel"] {
        assert!(
            wait_for_path_suffix(&server, endpoint, 1, Duration::from_millis(50))
                .await
                .is_empty(),
            "nothing was reserved, so {endpoint} must not be called"
        );
    }
}

/// A request the platform *blocked* must produce no usage record from either
/// path: the platform already holds the block, and a second BLOCKED (or a stray
/// ALLOWED) event would re-fire the owner "limit hit" email.
#[tokio::test]
async fn a_platform_block_reports_no_usage_from_either_path() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "allowed": false,
            "decision": {"policyId":"pol_strict","policyName":"Org cap","policyType":"COST_CAP",
                         "reason":"org cap reached"}
        })))
        .mount(&server)
        .await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let gs = strict_guard_state(&server.uri(), &bundle);
    let plugin = usage_plugin(&gs);
    let app = Router::new()
        .route("/v1/chat/completions", post(usage_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body =
        json!({"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert!(resp.headers().contains_key("x-noveum-guard-blocked"));
    // The telemetry layer fires for a synthetic block too — flagged as one.
    plugin
        .export_metrics(RequestMetrics {
            guard_blocked: true,
            ..telemetry_metrics("gpt-4o", 0, 0, 0.0)
        })
        .await
        .expect("export ok");

    let events = wait_for_usage(&server, &usage_path(), 1, Duration::from_millis(600)).await;
    assert!(
        events.is_empty(),
        "the platform blocked this call and already holds the record, got {events:?}"
    );
}

/// A stream whose provider reported no usage at all settles through `abandon`
/// (the estimate stays applied), which is still a metered record — the exporter
/// must not add a second one on top. (A stream that *does* report usage settles
/// through `complete` instead; see
/// `a_strict_openai_stream_completes_with_the_providers_real_usage`. Either way
/// the reservation is the single metered record.)
#[tokio::test]
async fn a_strict_stream_is_metered_once_through_its_abandoned_reservation() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-stream").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let gs = strict_guard_state(&server.uri(), &bundle);
    let plugin = usage_plugin(&gs);
    let app = Router::new()
        .route("/v1/chat/completions", post(sse_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","stream":true,"max_tokens":256,
        "messages":[{"role":"user","content":"hi"}]});
    let resp = app.oneshot(chat_request(&body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();

    // The telemetry layer reassembles the stream and reports its real usage.
    plugin
        .export_metrics(RequestMetrics {
            is_streaming: true,
            ..telemetry_metrics("gpt-4o-2024-11-20", 11, 4, 0.000035)
        })
        .await
        .expect("export ok");

    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(
        abandons.len(),
        1,
        "a stream with no recoverable usage keeps its estimate applied"
    );
    assert!(abandons[0].0.contains("/reservations/res-stream/"));
    let duplicates = allowed_usage_events(&server, Duration::from_millis(600)).await;
    assert!(
        duplicates.is_empty(),
        "the abandoned reservation already meters this stream, got {duplicates:?}"
    );
}

// --- THE HEADLINE: a 50-way burst must not exceed the platform's cap ---------

/// A wiremock `/admit` responder that models the platform counter honestly: one
/// shared atomic, charged with a compare-and-swap exactly as the real Lua does,
/// plus `requestId` idempotency (a replayed id never re-charges).
#[derive(Clone)]
struct CapModel(Arc<CapModelInner>);

struct CapModelInner {
    cap_micros: u64,
    spent_micros: AtomicU64,
    next_id: AtomicU64,
    /// `requestId` → reservation id, for idempotent replays.
    seen: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl CapModel {
    fn new(cap_usd: f64) -> Self {
        Self(Arc::new(CapModelInner {
            cap_micros: (cap_usd * 1_000_000.0).round() as u64,
            spent_micros: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
        }))
    }
    fn spent_usd(&self) -> f64 {
        self.0.spent_micros.load(Ordering::SeqCst) as f64 / 1_000_000.0
    }
    fn cap_usd(&self) -> f64 {
        self.0.cap_micros as f64 / 1_000_000.0
    }
}

impl wiremock::Respond for CapModel {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let request_id = body["requestId"].as_str().unwrap_or_default().to_string();
        let cost_micros =
            (body["estimatedCostUsd"].as_f64().unwrap_or(0.0) * 1_000_000.0).round() as u64;

        if let Some(id) = self.0.seen.lock().unwrap().get(&request_id) {
            // A replay of an already-admitted requestId: same reservation, no
            // second charge.
            return ResponseTemplate::new(200).set_body_json(json!({
                "allowed": true, "reservationId": id, "replayed": true
            }));
        }

        // Charge atomically, or refuse. This is the whole point of the exercise:
        // the decision and the increment happen in ONE indivisible step, so two
        // concurrent callers can never both see room for the last dollar.
        let mut cur = self.0.spent_micros.load(Ordering::SeqCst);
        loop {
            if cur + cost_micros > self.0.cap_micros {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "allowed": false,
                    "decision": {
                        "policyId":"pol_strict","policyName":"Org cap","policyType":"COST_CAP",
                        "scope":"org","dimension":"1d_rolling",
                        "limit": self.cap_usd(),
                        "observed": cur as f64 / 1_000_000.0,
                        "projected": (cur + cost_micros) as f64 / 1_000_000.0,
                        "reason":"cost cap would be exceeded"
                    }
                }));
            }
            match self.0.spent_micros.compare_exchange(
                cur,
                cur + cost_micros,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        let id = format!("res-{}", self.0.next_id.fetch_add(1, Ordering::SeqCst));
        self.0.seen.lock().unwrap().insert(request_id, id.clone());
        ResponseTemplate::new(200).set_body_json(json!({
            "allowed": true, "reservationId": id, "replayed": false
        }))
    }
}

/// Run `n` concurrent guarded requests through one independent gateway
/// instance, on its own tokio runtime, and report which of them were blocked.
///
/// Each instance gets its own engine, its own `PendingSpend` and its own
/// `AdmissionClient` — i.e. exactly the state a separate replica would have.
fn run_gateway_instance(
    server_uri: String,
    requests: usize,
    barrier: Arc<std::sync::Barrier>,
) -> std::thread::JoinHandle<Vec<bool>> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("instance runtime");
        let out = rt.block_on(async move {
            let bundle = translate_bundle(&strict_cost_cap_payload(1_000.0, true)).unwrap();
            let app = Router::new()
                .route("/v1/chat/completions", post(usage_handler))
                .layer(from_fn_with_state(
                    strict_guard_state(&server_uri, &bundle),
                    guard_middleware,
                ));
            let body = json!({
                "model":"gpt-4o","max_tokens":1000,
                "messages":[{"role":"user","content":"hi"}]
            });
            let futures: Vec<_> = (0..requests)
                .map(|_| {
                    let app = app.clone();
                    let body = body.clone();
                    async move {
                        let resp = app.oneshot(chat_request(&body)).await.unwrap();
                        let blocked = resp.headers().contains_key("x-noveum-guard-blocked");
                        // Drain so guards settle deterministically.
                        let _ = axum::body::to_bytes(resp.into_body(), 1 << 20).await;
                        blocked
                    }
                })
                .collect();
            futures_util::future::join_all(futures).await
        });
        // Both instances share one global `reqwest` client (and therefore one
        // connection pool). Tearing a runtime down while the other still has
        // requests in flight can kill a connection out from under it, so hold
        // both runtimes alive until every burst has finished.
        barrier.wait();
        out
    })
}

/// **The headline test.** Fifty concurrent requests, split across two
/// independent gateway instances, against one platform that models its counter
/// honestly. The admitted spend must never exceed the cap — which is precisely
/// what the per-process ledger could not guarantee.
///
/// RESIDUAL TOLERANCE. This asserts zero overshoot *of the estimate at
/// admission time*, which is the only thing the gateway controls. Real spend can
/// still exceed the cap, by bounded amounts, for reasons no client-side change
/// can fix:
///
/// 1. **Estimate-vs-actual drift.** Admission charges
///    `input + max_output_tokens` at list price. A call that returns *more*
///    expensive usage than predicted (a pricing tier the gateway's table lags
///    on, a provider surcharge, cache-write tokens) settles above its
///    reservation. Bounded per request, unbounded across a long burst if the
///    table is stale.
/// 2. **Settlement after the window rolls.** A reservation taken at the end of a
///    rolling window may `complete` after it has rolled, landing the real cost
///    in the next window while the old one already released the hold.
/// 3. **Abandoned reservations.** A stream that dies without a usage chunk keeps
///    its *estimate* applied (deliberately conservative) — that errs toward
///    over-counting, not overshoot, but it means the counter is not exact.
/// 4. **Clock skew / expiry.** A reservation the gateway never settles is
///    released by the platform at `expiresAt`; if the request was in fact served
///    and its usage arrived late, the cap is briefly under-counted.
///
/// So: exactly zero admission-time overshoot, and a real-spend overshoot bounded
/// by (worst-case per-request estimate error) x (requests in flight at the
/// moment the cap is reached). It is NOT zero, and this test does not claim it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_way_burst_across_two_instances_never_exceeds_the_platform_cap() {
    const INSTANCES: usize = 2;
    const PER_INSTANCE: usize = 25;

    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_settlement_ok(&server).await;
    // $0.10 of headroom: with ~$0.01 predicted per call this admits ~10 of 50.
    let model = CapModel::new(0.10);
    Mock::given(method("POST"))
        .and(match_path(admit_path()))
        .respond_with(model.clone())
        .mount(&server)
        .await;

    let barrier = Arc::new(std::sync::Barrier::new(INSTANCES));
    let handles: Vec<_> = (0..INSTANCES)
        .map(|_| run_gateway_instance(server.uri(), PER_INSTANCE, barrier.clone()))
        .collect();
    let results: Vec<Vec<bool>> = tokio::task::spawn_blocking(move || {
        handles
            .into_iter()
            .map(|h| h.join().expect("instance thread"))
            .collect()
    })
    .await
    .expect("join instances");

    let total: usize = results.iter().map(|r| r.len()).sum();
    assert_eq!(total, INSTANCES * PER_INSTANCE, "every request completed");
    let allowed = results.iter().flatten().filter(|b| !**b).count();
    let blocked = results.iter().flatten().filter(|b| **b).count();

    // What one request was charged, read from what the gateway actually sent.
    let admits = wait_for_path_suffix(&server, "/admit", total, Duration::from_secs(10)).await;
    let per_request = admits[0].1["estimatedCostUsd"].as_f64().unwrap();
    assert!(per_request > 0.0, "the burst must charge a real estimate");
    let expected_admits = (model.cap_usd() / per_request).floor() as usize;

    assert!(
        allowed < total,
        "the cap must actually bite: {allowed}/{total} admitted"
    );
    assert_eq!(
        allowed, expected_admits,
        "the burst must admit exactly what the cap affords ({expected_admits}), \
         not once per instance; allowed={allowed} blocked={blocked}"
    );
    // The invariant, stated in dollars: admitted spend never exceeds the cap.
    let admitted_spend = allowed as f64 * per_request;
    assert!(
        admitted_spend <= model.cap_usd() + 1e-9,
        "admitted ${admitted_spend} against a ${} cap",
        model.cap_usd()
    );
    assert!(
        model.spent_usd() <= model.cap_usd() + 1e-9,
        "the platform counter itself must never exceed the cap"
    );
    // Sanity: this really was two independent instances, both of which ran.
    assert!(results.iter().all(|r| r.len() == PER_INSTANCE));
}

// ---------------------------------------------------------------------------
// §6.2 — STRICT-MODE STREAMS MUST SETTLE ON REAL USAGE, NOT THE RESERVATION.
//
// A strict request reserves `input_tokens + max_output_tokens`. `max_tokens` is
// routinely 4096 while a real streamed reply is tens of tokens, so retaining the
// estimate (which is what `abandon` does, by design) bills a stream ~100x its
// true cost and eats the customer's cap accordingly. SSE usage IS recoverable —
// the final frame carries it — so these tests pin down that the gateway tees the
// body, reads it, and reconciles the reservation DOWN via `complete`, while the
// client still receives the identical bytes.
// ---------------------------------------------------------------------------

/// A realistic OpenAI stream: content deltas (one carrying multi-byte UTF-8)
/// then the `include_usage` frame the gateway forces on, then `[DONE]`.
const OPENAI_STREAM: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Héllo\"}}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" wörld 🌍\"}}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}}\n\n",
    "data: [DONE]\n\n",
);

/// The raw Anthropic dialect: usage split across `message_start` (input) and
/// `message_delta` (output).
const ANTHROPIC_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Héllo 🌍\"}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// A stream that never reports usage and stops mid-answer (provider truncation).
const TRUNCATED_STREAM: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half an ans\"}}]}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"wer\"}}]}\n\n",
);

/// Cut `raw` into `step`-byte transport chunks — the way an unlucky TCP flush
/// (or an adversarial provider) would, i.e. through the middle of the JSON and
/// of multi-byte characters.
fn split_every(raw: &str, step: usize) -> Vec<Bytes> {
    raw.as_bytes()
        .chunks(step.max(1))
        .map(Bytes::copy_from_slice)
        .collect()
}

/// An SSE response whose body yields exactly `chunks`, in order.
fn sse_response(chunks: Vec<Bytes>) -> Response {
    let stream = futures_util::stream::iter(chunks.into_iter().map(Ok::<Bytes, std::io::Error>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("static SSE response is valid")
}

/// A router that serves `raw` as SSE (split into `step`-byte chunks) behind the
/// strict guard middleware.
fn streaming_app(
    server_uri: &str,
    bundle: &PolicyBundle,
    raw: &'static str,
    step: usize,
) -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(move || async move { sse_response(split_every(raw, step)) }),
        )
        .layer(from_fn_with_state(
            strict_guard_state(server_uri, bundle),
            guard_middleware,
        ))
}

/// Every reservation settlement the gateway sent, as `(path, body)`.
async fn settlements(server: &MockServer) -> Vec<(String, Value)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            let p = r.url.path();
            p.ends_with("/complete") || p.ends_with("/abandon") || p.ends_with("/cancel")
        })
        .map(|r| {
            (
                r.url.path().to_string(),
                serde_json::from_slice(&r.body).unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// Drain a response body and return the exact chunk sequence the client saw.
async fn drain_chunks(resp: Response) -> Vec<Bytes> {
    use http_body_util::BodyExt;
    let mut body = resp.into_body();
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Some(d) = frame.expect("stream must not error").data_ref() {
            out.push(d.clone());
        }
    }
    out
}

fn joined(chunks: &[Bytes]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.to_vec()).collect()
}

/// A stream request with an explicit `max_tokens`, i.e. the shape whose
/// reservation is wildly larger than the real answer.
fn stream_request(model: &str, provider: &str, max_tokens: u64) -> Request<Body> {
    let body = json!({
        "model": model, "stream": true, "max_tokens": max_tokens,
        "messages":[{"role":"user","content":"hi"}]
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-provider", provider)
        .header("authorization", "Bearer test-provider-key")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// **The reproduction.** A strict-mode OpenAI stream reserves
/// `input + max_tokens` and must settle on the PROVIDER's counts — a `complete`
/// carrying 11/4, costing far less than the reservation — not an `abandon` that
/// retains 4096 output tokens' worth of estimate.
#[tokio::test]
async fn a_strict_openai_stream_completes_with_the_providers_real_usage() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-openai-stream").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    // Frames deliberately split at 7-byte boundaries: usage recovery must not
    // depend on the provider's flush pattern.
    let app = streaming_app(&server.uri(), &bundle, OPENAI_STREAM, 7);

    let resp = app
        .oneshot(stream_request("gpt-4o", "openai", 4096))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let client_bytes = joined(&drain_chunks(resp).await);
    assert_eq!(
        client_bytes,
        OPENAI_STREAM.as_bytes(),
        "metering must not alter a single byte the client receives"
    );

    let estimated = wait_for_path_suffix(&server, "/admit", 1, Duration::from_secs(5)).await[0].1
        ["estimatedCostUsd"]
        .as_f64()
        .unwrap();

    let completes = wait_for_path_suffix(&server, "/complete", 1, Duration::from_secs(5)).await;
    assert_eq!(completes.len(), 1, "the stream's usage IS recoverable");
    assert!(completes[0].0.contains("/reservations/res-openai-stream/"));
    let c = &completes[0].1;
    assert_eq!(
        c["inputTokens"], 11,
        "the provider's count, not the estimate"
    );
    assert_eq!(c["outputTokens"], 4, "NOT the 4096 that was reserved");
    assert_eq!(c["model"], "gpt-4o");
    assert_eq!(c["requestCount"], 1);

    // The whole point: the settled cost reconciles the reservation DOWN.
    let settled = c["costUsd"].as_f64().unwrap();
    assert!(settled > 0.0, "a real call is never free");
    assert!(
        settled < estimated,
        "settled ${settled} must be below the ${estimated} reservation"
    );
    assert!(
        settled * 10.0 < estimated,
        "a 4096-token reservation for a 4-token answer must shrink by orders of \
         magnitude; settled ${settled} vs reserved ${estimated}"
    );

    // ...and it settled exactly once, through `complete` alone.
    let all = settlements(&server).await;
    assert_eq!(all.len(), 1, "no double settlement: {all:?}");
}

/// The same guarantee for Anthropic, whose usage arrives split across two events
/// (`message_start` input, `message_delta` output) rather than in one frame.
#[tokio::test]
async fn a_strict_anthropic_stream_completes_with_the_providers_real_usage() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-anthropic-stream").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = streaming_app(&server.uri(), &bundle, ANTHROPIC_STREAM, 13);

    let resp = app
        .oneshot(stream_request("claude-sonnet-4-5", "anthropic", 4096))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        joined(&drain_chunks(resp).await),
        ANTHROPIC_STREAM.as_bytes(),
        "the Anthropic dialect must reach the client verbatim"
    );

    let estimated = wait_for_path_suffix(&server, "/admit", 1, Duration::from_secs(5)).await[0].1
        ["estimatedCostUsd"]
        .as_f64()
        .unwrap();

    let completes = wait_for_path_suffix(&server, "/complete", 1, Duration::from_secs(5)).await;
    assert_eq!(completes.len(), 1);
    assert!(completes[0]
        .0
        .contains("/reservations/res-anthropic-stream/"));
    let c = &completes[0].1;
    assert_eq!(c["inputTokens"], 11, "from message_start");
    assert_eq!(
        c["outputTokens"], 4,
        "from message_delta — NOT the placeholder 1 on message_start, \
         and NOT the 4096 reserved"
    );
    let settled = c["costUsd"].as_f64().unwrap();
    assert!(settled > 0.0 && settled * 10.0 < estimated);
    assert_eq!(
        settlements(&server).await.len(),
        1,
        "exactly one settlement"
    );
}

/// The case where erring high is genuinely right: the provider truncated before
/// reporting anything. There is nothing authoritative to reconcile with, so the
/// conservative estimate must STAY applied — `abandon`, never a fabricated
/// `complete` (which, at output = 0, would release the entire hold).
#[tokio::test]
async fn a_stream_that_dies_mid_flight_without_usage_retains_the_estimate() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-truncated").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = streaming_app(&server.uri(), &bundle, TRUNCATED_STREAM, 5);

    let resp = app
        .oneshot(stream_request("gpt-4o", "openai", 4096))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        joined(&drain_chunks(resp).await),
        TRUNCATED_STREAM.as_bytes(),
        "a truncated stream still reaches the client exactly as sent"
    );

    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(abandons.len(), 1, "no usage → the estimate stays applied");
    assert!(abandons[0].0.contains("/reservations/res-truncated/"));
    assert!(abandons[0].1["reason"]
        .as_str()
        .is_some_and(|r| !r.is_empty()));
    let all = settlements(&server).await;
    assert_eq!(
        all.len(),
        1,
        "abandon and complete must never both fire: {all:?}"
    );
    assert!(
        !all[0].0.ends_with("/complete"),
        "the gateway must not invent token counts it never saw"
    );
}

/// The same, for an Anthropic stream cut after `message_start`: input tokens are
/// known but output tokens are not, which is exactly the half that decides the
/// bill. Completing at output = 0 would release the whole reservation.
#[tokio::test]
async fn an_input_only_report_is_not_enough_to_complete() {
    const HALF: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":900}}}\n\n";
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-halfusage").await;
    mount_settlement_ok(&server).await;

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = streaming_app(&server.uri(), &bundle, HALF, 9);
    let resp = app
        .oneshot(stream_request("claude-sonnet-4-5", "anthropic", 4096))
        .await
        .unwrap();
    let _ = drain_chunks(resp).await;

    let all = settlements(&server).await;
    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(abandons.len(), 1, "settled: {all:?}");
    assert_eq!(settlements(&server).await.len(), 1);
}

/// A client that walks away mid-stream: the usage frame never arrives, so the
/// estimate is retained — and, critically, the reservation settles EXACTLY ONCE
/// rather than being left to expire or being settled twice.
#[tokio::test]
async fn a_client_disconnecting_mid_stream_settles_exactly_once() {
    use http_body_util::BodyExt;

    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-client-gone").await;
    mount_settlement_ok(&server).await;

    // An upstream that hands over one frame and then stalls: the usage frame is
    // still "in flight" when the client vanishes.
    let stalling = || async {
        let stream = futures_util::stream::unfold(0usize, |i| async move {
            if i == 0 {
                let first = Bytes::from_static(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                );
                Some((Ok::<Bytes, std::io::Error>(first), 1))
            } else {
                tokio::time::sleep(Duration::from_secs(30)).await;
                None
            }
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    };

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(stalling))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    {
        let resp = app
            .oneshot(stream_request("gpt-4o", "openai", 4096))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let first = body.frame().await.expect("first frame").expect("no error");
        assert!(first
            .data_ref()
            .is_some_and(|d| d.starts_with(b"data: {\"choices\"")));
        // ...and the client goes away, dropping the body mid-stream.
    }

    let abandons = wait_for_path_suffix(&server, "/abandon", 1, Duration::from_secs(5)).await;
    assert_eq!(
        abandons.len(),
        1,
        "a disconnect mid-stream must still settle, conservatively"
    );
    assert!(abandons[0].0.contains("/reservations/res-client-gone/"));
    // Give any second settlement a chance to show up before asserting there
    // isn't one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let all = settlements(&server).await;
    assert_eq!(all.len(), 1, "settled more than once: {all:?}");
}

/// Byte-exactness under adversarial framing: whatever offsets the provider
/// flushes at, the client receives the identical byte sequence — and the
/// identical chunk sequence — whether or not metering is active.
#[tokio::test]
async fn metering_is_byte_exact_for_frames_split_at_arbitrary_offsets() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-byte-exact").await;
    mount_settlement_ok(&server).await;
    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();

    // 1 byte at a time cuts through every multi-byte character and JSON literal;
    // the others land mid-frame at unrelated offsets.
    for step in [1usize, 2, 3, 17, 64, OPENAI_STREAM.len()] {
        // Unmetered: the same handler with no guard layer at all.
        let plain = Router::new().route(
            "/v1/chat/completions",
            post(move || async move { sse_response(split_every(OPENAI_STREAM, step)) }),
        );
        let want = drain_chunks(
            plain
                .oneshot(stream_request("gpt-4o", "openai", 4096))
                .await
                .unwrap(),
        )
        .await;

        let metered = streaming_app(&server.uri(), &bundle, OPENAI_STREAM, step);
        let got = drain_chunks(
            metered
                .oneshot(stream_request("gpt-4o", "openai", 4096))
                .await
                .unwrap(),
        )
        .await;

        assert_eq!(
            joined(&got),
            OPENAI_STREAM.as_bytes(),
            "metering corrupted the stream at step {step}"
        );
        assert_eq!(
            got, want,
            "metering changed the chunk sequence at step {step}"
        );
    }

    // ...and every one of those requests still settled on the real usage.
    let completes = wait_for_path_suffix(&server, "/complete", 6, Duration::from_secs(5)).await;
    assert_eq!(completes.len(), 6, "one completion per metered request");
    for (_, c) in &completes {
        assert_eq!(c["inputTokens"], 11);
        assert_eq!(c["outputTokens"], 4);
    }
    assert!(
        wait_for_path_suffix(&server, "/abandon", 1, Duration::from_millis(50))
            .await
            .is_empty()
    );
}

/// Backpressure: the tee inspects chunks as they pass, it does not accumulate
/// the response. A frame the provider emits immediately must reach the client
/// immediately, even though a later frame is still seconds away.
#[tokio::test]
async fn metering_does_not_buffer_the_stream_or_delay_the_client() {
    let server = MockServer::start().await;
    mount_state_zero(&server).await;
    mount_usage_ok(&server).await;
    mount_admit_allowed(&server, "res-backpressure").await;
    mount_settlement_ok(&server).await;

    // Frame 1 now; frame 2 (carrying usage) only after a long pause.
    let slow_stream = || async {
        let stream = futures_util::stream::unfold(0usize, |i| async move {
            match i {
                0 => Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"first\"}}]}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    Some((
                        Ok(Bytes::from_static(
                            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n",
                        )),
                        2,
                    ))
                }
                _ => None,
            }
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    };

    let bundle = translate_bundle(&strict_cost_cap_payload(100.0, true)).unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(slow_stream))
        .layer(from_fn_with_state(
            strict_guard_state(&server.uri(), &bundle),
            guard_middleware,
        ));

    use http_body_util::BodyExt;
    let started = std::time::Instant::now();
    let resp = app
        .oneshot(stream_request("gpt-4o", "openai", 4096))
        .await
        .unwrap();
    let mut body = resp.into_body();
    let first = body.frame().await.expect("first frame").expect("no error");
    let elapsed = started.elapsed();
    assert!(
        first
            .data_ref()
            .is_some_and(|d| d.starts_with(b"data: {\"choices\"")),
        "the first frame must arrive as sent"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "the first frame waited {elapsed:?} — metering is buffering the response \
         instead of teeing it"
    );

    // Draining the rest still yields the real usage at settlement. (Settlement
    // is keyed to the body's lifetime, so the client has to let go of it — same
    // as a real connection closing.)
    while let Some(f) = body.frame().await {
        f.expect("no error");
    }
    drop(body);
    let completes = wait_for_path_suffix(&server, "/complete", 1, Duration::from_secs(5)).await;
    assert_eq!(completes.len(), 1);
    assert_eq!(completes[0].1["outputTokens"], 4);
}

// ===========================================================================
// Shared-gateway tenancy (§6.4 / NOV-117)
// ===========================================================================
//
// These tests stand up ONE mock platform serving TWO organizations with TWO
// projects each, and drive the real router stack (`tenant_middleware` wrapping
// `guard_middleware`). Isolation is asserted three ways, because any one of
// them alone could pass while the others leak:
//
//  1. *Behavior* — identical policies + identical models, different counters:
//     org A blocks, org B is served.
//  2. *Wire* — every platform stub is matched on BOTH the project path AND the
//     caller's bearer token, so a request made for the wrong tenant, or with
//     the wrong tenant's credential, matches nothing and 404s (which the
//     gateway surfaces as a failure rather than silently succeeding).
//  3. *State* — one tenant's pending reservations and cached live state cannot
//     move another tenant's admission decision.

use noveum_ai_gateway::policy::middleware::{tenant_middleware, SharedTenancy};
use noveum_ai_gateway::policy::remote::{SharedTenancyConfig, TENANT_CREDENTIAL_HEADER};

const ORG_A: &str = "org_alpha";
const ORG_B: &str = "org_beta";
const KEY_A: &str = "nv_key_alpha";
const KEY_B: &str = "nv_key_beta";
/// A third organization with exactly ONE project, for the "no routing header"
/// case (a key entitled to several projects has nothing to default to).
const ORG_SOLO: &str = "org_solo";
const KEY_SOLO: &str = "nv_key_solo";

fn policies_path(project: &str) -> String {
    format!("/api/v1/projects/{project}/policies/effective")
}
fn tenant_state_path(project: &str) -> String {
    format!("/api/v1/projects/{project}/policies/state")
}
fn tenant_usage_path(project: &str) -> String {
    format!("/api/v1/projects/{project}/policies/usage")
}
fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// Stub the platform's project listing — the call that establishes identity.
/// Matched on the bearer token, so each key sees only its own organization.
async fn mount_identity(server: &MockServer, key: &str, org: &str, projects: &[&str]) {
    let body: Vec<Value> = projects
        .iter()
        .map(|p| json!({"id": p, "name": p, "organizationId": org}))
        .collect();
    Mock::given(method("GET"))
        .and(match_path("/api/v1/projects"))
        .and(match_header("authorization", bearer(key).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Stub one project's policy set, live state and usage sink — all matched on
/// the owning key's bearer token. Any call made under a different credential
/// matches no stub and 404s, which is exactly the signal a cross-tenant leak
/// would produce.
async fn mount_project(
    server: &MockServer,
    key: &str,
    project: &str,
    policies: Value,
    state: Value,
) {
    Mock::given(method("GET"))
        .and(match_path(policies_path(project)))
        .and(match_header("authorization", bearer(key).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(policies))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(match_path(tenant_state_path(project)))
        .and(match_header("authorization", bearer(key).as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(state))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(match_path(tenant_usage_path(project)))
        .and(match_header("authorization", bearer(key).as_str()))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({"success":true,"accepted":1,"persisted":1,"blocked":0})),
        )
        .mount(server)
        .await;
}

/// A `cost_cap` at `max_usd`. Deliberately byte-identical across tenants so the
/// cache-keying test cannot pass by the policies happening to differ.
fn shared_cost_cap(max_usd: f64) -> Value {
    json!({"policies":[{
        "policyId":"pol_cap","name":"Daily cap","type":"COST_CAP",
        "enabled":true,"failClosed":true,
        "config":{"window":"1d_rolling","maxUsd":max_usd,"action":"BLOCK"}
    }]})
}

fn spend_state(cost_usd: f64) -> Value {
    json!({"cost":{"1d_rolling":cost_usd},"rate":{"requests_1m":0},"stale":false,"ttlSeconds":30})
}

/// The real shared-gateway stack: tenancy layer outside, guard layer inside,
/// and a process-wide `GuardState` that is an EMPTY no-op engine — exactly what
/// `main.rs` builds in shared mode. If the tenancy layer ever failed to inject
/// a tenant, the guard layer would enforce nothing, so every "blocked" assertion
/// below is also a proof that injection happened.
fn shared_stack(server_uri: &str) -> (Router, Arc<SharedTenancy>) {
    let cfg = SharedTenancyConfig {
        base_url: server_uri.trim_end_matches('/').to_string(),
        resolution_ttl: Duration::from_secs(300),
        max_tenants: 64,
    };
    let shared = Arc::new(SharedTenancy::new(cfg, backed_opts(), None, false));
    let process_wide = GuardState {
        engine: Arc::new(PolicyEngine::from_bundle(
            &noveum_ai_gateway::policy::PolicyBundle::default(),
            EngineOptions::default(),
        )),
        live: None,
        usage: None,
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .layer(from_fn_with_state(process_wide, guard_middleware))
        .layer(from_fn_with_state(shared.clone(), tenant_middleware));
    (app, shared)
}

/// One chat request, optionally carrying a credential and routing headers.
fn tenant_request(key: Option<&str>, project: Option<&str>, org: Option<&str>) -> Request<Body> {
    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hello there"}]});
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-provider", "openai")
        .header("authorization", "Bearer sk-provider-test");
    if let Some(k) = key {
        b = b.header(TENANT_CREDENTIAL_HEADER, k);
    }
    if let Some(p) = project {
        b = b.header("x-project-id", p);
    }
    if let Some(o) = org {
        b = b.header("x-organization-id", o);
    }
    b.body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn blocked(resp: &Response) -> bool {
    resp.headers().contains_key("x-noveum-guard-blocked")
}

async fn body_string(resp: Response) -> String {
    use http_body_util::BodyExt;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Every platform path touched under a given bearer token.
async fn paths_for_key(server: &MockServer, key: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == bearer(key))
        })
        .map(|r| r.url.path().to_string())
        .collect()
}

#[tokio::test]
async fn shared_mode_isolates_two_organizations_with_two_projects_each() {
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;

    // Two organizations, two projects each. Every project carries the SAME
    // cost cap; only the spend differs, and only in org A's first project.
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;
    mount_identity(&server, KEY_B, ORG_B, &["proj_b1", "proj_b2"]).await;
    mount_project(
        &server,
        KEY_A,
        "proj_a1",
        shared_cost_cap(10.0),
        spend_state(25.0),
    )
    .await;
    mount_project(
        &server,
        KEY_A,
        "proj_a2",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;
    mount_project(
        &server,
        KEY_B,
        "proj_b1",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;
    mount_project(
        &server,
        KEY_B,
        "proj_b2",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;

    let (app, shared) = shared_stack(&server.uri());

    // Org A / project 1 is over its cap → blocked.
    let a1 = app
        .clone()
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert!(blocked(&a1), "org A's over-cap project must be blocked");

    // Org B / project 1: identical policy, identical model, its own counters →
    // served. A shared cache keyed by anything but the tenant would have
    // blocked this too.
    let b1 = app
        .clone()
        .oneshot(tenant_request(Some(KEY_B), Some("proj_b1"), None))
        .await
        .unwrap();
    assert!(
        !blocked(&b1),
        "org B must not inherit org A's spend: {}",
        body_string(b1).await
    );

    // The second project of each org is likewise independent.
    let a2 = app
        .clone()
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a2"), None))
        .await
        .unwrap();
    assert!(!blocked(&a2), "org A's OTHER project has its own counters");
    let b2 = app
        .clone()
        .oneshot(tenant_request(Some(KEY_B), Some("proj_b2"), None))
        .await
        .unwrap();
    assert!(!blocked(&b2));

    // Four distinct tenants are warm, none shared.
    assert_eq!(shared.warm_tenants(), 4);

    // On the wire: org A's credential never touched a project of org B, and
    // vice versa. This is the assertion that would fail on a cache keyed by a
    // client-supplied header.
    let a_paths = paths_for_key(&server, KEY_A).await;
    assert!(
        a_paths.iter().all(|p| !p.contains("proj_b")),
        "org A's key reached an org B project: {a_paths:?}"
    );
    assert!(a_paths.iter().any(|p| p.contains("proj_a1")));
    let b_paths = paths_for_key(&server, KEY_B).await;
    assert!(
        b_paths.iter().all(|p| !p.contains("proj_a")),
        "org B's key reached an org A project: {b_paths:?}"
    );

    // The BLOCKED usage event was reported against org A's project only.
    let a_events = wait_for_usage(
        &server,
        &tenant_usage_path("proj_a1"),
        1,
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(a_events.len(), 1, "{a_events:?}");
    assert_eq!(a_events[0]["blockedBy"], "COST_CAP");
    for other in ["proj_a2", "proj_b1", "proj_b2"] {
        let events = wait_for_usage(
            &server,
            &tenant_usage_path(other),
            usize::MAX,
            Duration::from_millis(150),
        )
        .await;
        assert!(
            events.iter().all(|e| e["status"] != "BLOCKED"),
            "a block leaked into {other}: {events:?}"
        );
    }
}

#[tokio::test]
async fn shared_mode_keeps_reservations_per_tenant() {
    // Reservations are the other half of "counters": a request admitted for org
    // B must not consume org A's remaining allowance. Both tenants get the same
    // `maxRequests: 1` policy over frozen zero counters, so the ledger is the
    // only thing that can block.
    std::env::set_var("NOVEUM_GUARD_USAGE_FLUSH_MS", "40");
    let server = MockServer::start().await;
    let one_per_minute = json!({"policies":[{
        "policyId":"pol_rl","name":"one per minute","type":"RATE_LIMIT",
        "enabled":true,"failClosed":true,
        "config":{"windows":[{"period":"1m","maxRequests":1,"action":"BLOCK"}]}
    }]});
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;
    mount_identity(&server, KEY_B, ORG_B, &["proj_b1", "proj_b2"]).await;
    mount_project(
        &server,
        KEY_A,
        "proj_a1",
        one_per_minute.clone(),
        spend_state(0.0),
    )
    .await;
    mount_project(&server, KEY_B, "proj_b1", one_per_minute, spend_state(0.0)).await;

    let (app, _shared) = shared_stack(&server.uri());
    let send = |key: &'static str, project: &'static str| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(tenant_request(Some(key), Some(project), None))
                .await
                .unwrap();
            let was_blocked = blocked(&resp);
            // Drain the body so the reservation completes, as a real client would.
            let _ = body_string(resp).await;
            was_blocked
        }
    };

    // Org B burns its single request, then hits its own limit.
    assert!(
        !send(KEY_B, "proj_b1").await,
        "org B's first request passes"
    );
    assert!(
        send(KEY_B, "proj_b1").await,
        "org B's second request must see its own reservation"
    );
    // Org A's allowance is untouched by any of that.
    assert!(
        !send(KEY_A, "proj_a1").await,
        "org B's traffic must not consume org A's rate allowance"
    );
    // ...and org A then limits itself, proving its ledger is real, not absent.
    assert!(
        send(KEY_A, "proj_a1").await,
        "org A's second request must see org A's own reservation"
    );
}

#[tokio::test]
async fn shared_mode_rejects_a_project_the_credential_is_not_entitled_to() {
    let server = MockServer::start().await;
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;
    mount_identity(&server, KEY_B, ORG_B, &["proj_b1", "proj_b2"]).await;
    mount_project(
        &server,
        KEY_B,
        "proj_b1",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;

    let (app, shared) = shared_stack(&server.uri());
    // Org A's key pointing `x-project-id` at an org B project. This is the
    // header-spoofing case: it is an ERROR, never a silent override.
    let resp = app
        .clone()
        .oneshot(tenant_request(Some(KEY_A), Some("proj_b1"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_string(resp).await;
    assert!(body.contains("project_not_entitled"), "{body}");
    assert!(body.contains("proj_b1"), "{body}");

    // Nothing was warmed, and org A's key never touched an org B path.
    assert_eq!(shared.warm_tenants(), 0);
    let a_paths = paths_for_key(&server, KEY_A).await;
    assert!(
        a_paths.iter().all(|p| !p.contains("proj_b1")),
        "the refused project was still fetched: {a_paths:?}"
    );

    // A project that exists nowhere gets the identical answer, so the status
    // code discloses nothing about other organizations' projects.
    let resp = app
        .clone()
        .oneshot(tenant_request(Some(KEY_A), Some("proj_ghost"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A spoofed ORGANIZATION header is refused the same way, even when paired
    // with a project the key really does own.
    let resp = app
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), Some(ORG_B)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_string(resp).await;
    assert!(body.contains("organization_mismatch"), "{body}");
}

#[tokio::test]
async fn shared_mode_without_a_routing_header_resolves_to_the_credentials_own_project() {
    let server = MockServer::start().await;
    mount_identity(&server, KEY_SOLO, ORG_SOLO, &["proj_solo"]).await;
    mount_project(
        &server,
        KEY_SOLO,
        "proj_solo",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;
    // A second organization exists and must never be reachable by accident.
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;

    let (app, shared) = shared_stack(&server.uri());
    let resp = app
        .clone()
        .oneshot(tenant_request(Some(KEY_SOLO), None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!blocked(&resp));
    assert_eq!(shared.warm_tenants(), 1);
    let paths = paths_for_key(&server, KEY_SOLO).await;
    assert!(
        paths.iter().any(|p| p.contains("proj_solo")),
        "the key's own project was not used: {paths:?}"
    );

    // A key entitled to SEVERAL projects and no routing header is refused
    // rather than guessed at — guessing would meter one project's spend
    // against another project's budget.
    let resp = app
        .oneshot(tenant_request(Some(KEY_A), None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_string(resp).await;
    assert!(body.contains("project_id_required"), "{body}");
}

#[tokio::test]
async fn shared_mode_fails_closed_on_an_unusable_credential() {
    let server = MockServer::start().await;
    // A valid tenant exists — the point is that a bad credential never lands on it.
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;
    mount_project(
        &server,
        KEY_A,
        "proj_a1",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;
    // Anything else presenting itself at the identity endpoint is rejected,
    // exactly as the platform rejects an unknown, expired or revoked key.
    Mock::given(method("GET"))
        .and(match_path("/api/v1/projects"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"success":false,"error":{"code":"INVALID_API_KEY","message":"invalid"}}),
        ))
        .mount(&server)
        .await;

    let (app, shared) = shared_stack(&server.uri());

    // No credential at all.
    let resp = app
        .clone()
        .oneshot(tenant_request(None, None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_string(resp).await;
    assert!(body.contains("missing_noveum_credential"), "{body}");

    // A present-but-empty header is not a credential either.
    let resp = app
        .clone()
        .oneshot(tenant_request(Some("   "), None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // An expired/revoked key. Note it also names an entitled-looking project:
    // the routing header must not rescue an unauthenticated caller.
    let resp = app
        .clone()
        .oneshot(tenant_request(Some("nv_expired"), Some("proj_a1"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_string(resp).await;
    assert!(body.contains("invalid_noveum_credential"), "{body}");

    // Nothing was warmed and nothing was fetched under the bad credential —
    // in particular there is no default tenant to have fallen through to.
    assert_eq!(shared.warm_tenants(), 0);
    let leaked = paths_for_key(&server, "nv_expired").await;
    assert!(
        leaked.iter().all(|p| !p.contains("/policies/")),
        "an unauthenticated caller reached a project: {leaked:?}"
    );

    // The valid tenant still works, so the refusals above are not a blanket outage.
    let resp = app
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn shared_mode_fails_closed_when_identity_cannot_be_verified() {
    let server = MockServer::start().await;
    // The platform is up but broken (5xx). "Unknown" must never mean "allowed",
    // and must never be cached as a denial either.
    Mock::given(method("GET"))
        .and(match_path("/api/v1/projects"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;

    let (app, shared) = shared_stack(&server.uri());
    let resp = app
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_string(resp).await;
    assert!(body.contains("tenant_resolution_unavailable"), "{body}");
    assert_eq!(shared.warm_tenants(), 0);
}

#[tokio::test]
async fn shared_mode_fails_closed_when_a_tenants_policies_cannot_be_fetched() {
    // Identity resolves, but the tenant's own policy set does not. With no
    // known policy set the request would be forwarded unguarded, so it is
    // refused — the same rule dedicated startup applies, per tenant.
    let server = MockServer::start().await;
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1"]).await;
    Mock::given(method("GET"))
        .and(match_path(policies_path("proj_a1")))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let (app, shared) = shared_stack(&server.uri());
    let resp = app
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(shared.warm_tenants(), 1, "the slot exists but stays empty");
}

#[tokio::test]
async fn shared_mode_never_shares_a_cache_entry_between_tenants() {
    // The narrow cache-keying claim, isolated: two tenants asking for the SAME
    // model under a byte-identical policy document. If any cache — policies,
    // live state, reservations, usage sink — were keyed by the model, the
    // policy shape, or a request header rather than the derived tenant, the
    // second caller would inherit the first's verdict.
    let server = MockServer::start().await;
    mount_identity(&server, KEY_A, ORG_A, &["proj_a1", "proj_a2"]).await;
    mount_identity(&server, KEY_B, ORG_B, &["proj_b1", "proj_b2"]).await;
    // Identical caps; only the spend behind them differs.
    mount_project(
        &server,
        KEY_A,
        "proj_a1",
        shared_cost_cap(5.0),
        spend_state(99.0),
    )
    .await;
    mount_project(
        &server,
        KEY_B,
        "proj_b1",
        shared_cost_cap(5.0),
        spend_state(0.0),
    )
    .await;

    let (app, _shared) = shared_stack(&server.uri());
    // Warm A first, so any shared entry would be A's.
    let a = app
        .clone()
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert!(blocked(&a));
    let b = app
        .clone()
        .oneshot(tenant_request(Some(KEY_B), Some("proj_b1"), None))
        .await
        .unwrap();
    assert!(!blocked(&b), "org B inherited org A's cached live state");
    // ...and again in the other order, so neither direction leaks.
    let b = app
        .clone()
        .oneshot(tenant_request(Some(KEY_B), Some("proj_b1"), None))
        .await
        .unwrap();
    assert!(!blocked(&b));
    let a = app
        .oneshot(tenant_request(Some(KEY_A), Some("proj_a1"), None))
        .await
        .unwrap();
    assert!(blocked(&a), "org A stopped blocking after org B was served");

    // Both tenants' state endpoints were really consulted — a single shared
    // entry would show up as one of them never being fetched.
    let reqs = server.received_requests().await.unwrap_or_default();
    for project in ["proj_a1", "proj_b1"] {
        assert!(
            reqs.iter()
                .any(|r| r.url.path() == tenant_state_path(project)),
            "no live-state fetch for {project}"
        );
    }
}

#[tokio::test]
async fn shared_mode_never_forwards_the_tenant_credential_upstream() {
    // The credential is a platform secret. It must not reach the model provider,
    // and nothing downstream of the tenancy layer has any business reading it.
    let server = MockServer::start().await;
    mount_identity(&server, KEY_SOLO, ORG_SOLO, &["proj_solo"]).await;
    mount_project(
        &server,
        KEY_SOLO,
        "proj_solo",
        shared_cost_cap(10.0),
        spend_state(0.0),
    )
    .await;

    async fn echo_headers(req: Request<Body>) -> Response {
        let names: Vec<String> = req
            .headers()
            .keys()
            .map(|k| k.as_str().to_string())
            .collect();
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&json!({"headers": names})).unwrap(),
        )
            .into_response()
    }

    let cfg = SharedTenancyConfig {
        base_url: server.uri().trim_end_matches('/').to_string(),
        resolution_ttl: Duration::from_secs(300),
        max_tenants: 64,
    };
    let shared = Arc::new(SharedTenancy::new(cfg, backed_opts(), None, false));
    let process_wide = GuardState {
        engine: Arc::new(PolicyEngine::from_bundle(
            &noveum_ai_gateway::policy::PolicyBundle::default(),
            EngineOptions::default(),
        )),
        live: None,
        usage: None,
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_headers))
        .layer(from_fn_with_state(process_wide, guard_middleware))
        .layer(from_fn_with_state(shared, tenant_middleware));

    let resp = app
        .oneshot(tenant_request(Some(KEY_SOLO), None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        !body.contains(TENANT_CREDENTIAL_HEADER),
        "the tenant credential header reached the upstream handler: {body}"
    );
    assert!(!body.contains(KEY_SOLO), "{body}");
    // The routing header is still forwarded — it is legitimate attribution
    // metadata for telemetry, it is just never identity.
    assert!(body.contains("x-provider"), "{body}");
}

#[tokio::test]
async fn shared_mode_leaves_unguarded_paths_open() {
    // Liveness/readiness probes must not need a tenant credential.
    let server = MockServer::start().await;
    let (app, _shared) = shared_stack(&server.uri());
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn dedicated_mode_is_untouched_by_the_tenancy_layer() {
    // The regression guard for mode 1: with no tenancy layer wired, the guard
    // middleware sees the process-wide state and behaves exactly as before —
    // no credential required, no routing header consulted.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(match_path(state_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(spend_state(25.0)))
        .mount(&server)
        .await;
    mount_usage_ok(&server).await;
    let c = cfg(&server.uri());
    let bundle = translate_bundle(&shared_cost_cap(10.0)).unwrap();
    let gs = GuardState {
        engine: Arc::new(PolicyEngine::from_bundle(&bundle, backed_opts())),
        live: Some(Arc::new(RemoteLiveState::new(c.clone()))),
        usage: Some(UsageReporter::spawn(c)),
        pending: Arc::new(noveum_ai_gateway::policy::remote::PendingSpend::new()),
        admission: None,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    // No credential header anywhere, and a wholly bogus routing header: the
    // dedicated project is enforced regardless, exactly as documented.
    let resp = app
        .oneshot(tenant_request(
            None,
            Some("some-other-project"),
            Some("some-other-org"),
        ))
        .await
        .unwrap();
    assert!(
        blocked(&resp),
        "the process-wide project must still be enforced"
    );
    let reqs = server.received_requests().await.unwrap_or_default();
    assert!(
        reqs.iter().all(|r| r.url.path().contains(PROJECT)),
        "a routing header changed the project in dedicated mode"
    );
}
