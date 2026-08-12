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
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
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
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
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
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
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
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(slow_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
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
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(echo_handler))
        .layer(from_fn_with_state(gs, guard_middleware));

    for (field, value) in [
        ("max_tokens", json!(u64::MAX)),
        ("max_completion_tokens", json!(u64::MAX)),
        ("max_output_tokens", json!(u64::MAX)),
        ("max_tokens", json!(-1)),
        ("max_tokens", json!(10_000_001u64)),
    ] {
        let mut body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
        body[field] = value.clone();
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
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
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(echo_handler))
            .layer(from_fn_with_state(gs, guard_middleware));

        let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
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
