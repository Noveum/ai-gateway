//! Noveum AI Gateway — library crate.
//!
//! This crate is split into a library (`lib.rs`) and a thin binary (`main.rs`)
//! so that every subsystem — providers, proxy, telemetry, and the Nova Guard
//! policy-enforcement layer — is unit- and integration-testable without binding
//! a TCP port.
//!
//! The binary is a small bootstrap that builds the [`AppState`], constructs the
//! router via [`build_router`], and serves it. Tests construct the same router
//! and drive it with `tower::ServiceExt::oneshot`.

// These clippy lints are pre-existing in the original gateway code (the provider
// metrics extractors, the proxy layer, and the telemetry middleware) and are out
// of scope for the Nova Guard change. The new Nova Guard modules are clippy-clean
// under `-D warnings`; allowing these crate-wide avoids churning untouched,
// logic-heavy pre-existing functions.
// NOV-132: the crate ships with `panic = "abort"`, so a panic on a request path
// does not fail one request, it kills the process and drops every in-flight
// request on that replica. Deny the two ways that happens by accident; each
// remaining site carries an `#[allow]` with the reason it is safe, so the
// argument is re-checked on every build instead of living in a comment.
// `not(test)` keeps the `#[cfg(test)]` modules, where panicking IS the
// assertion mechanism, unaffected. `src/main.rs` is a separate crate root and
// is deliberately not covered: startup is where an abort is correct.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::borrowed_box)]
#![allow(clippy::redundant_closure)]

// Shared core: compiles to BOTH the native server and the wasm32 Cloudflare
// Worker, so guardrails + request routing behave identically on every deployment
// shape.
pub mod policy;
pub mod routing;
pub mod sigv4;

/// Shared metering primitives — SSE frame reassembly and response/stream usage
/// parsing — re-exported at the crate root because they are consumed by all
/// three deployment shapes (native telemetry, the Anthropic stream translator,
/// and the Cloudflare Worker bridge). Compiles on native AND wasm32.
pub use policy::metering;

// Native runtime (Tokio + Axum server): the binary, Docker image, and library
// server. Not compiled for the wasm32 (Cloudflare Worker) target.
#[cfg(not(target_arch = "wasm32"))]
pub mod config;
#[cfg(not(target_arch = "wasm32"))]
pub mod error;
#[cfg(not(target_arch = "wasm32"))]
pub mod handlers;
#[cfg(not(target_arch = "wasm32"))]
pub mod providers;
#[cfg(not(target_arch = "wasm32"))]
pub mod proxy;
#[cfg(not(target_arch = "wasm32"))]
pub mod telemetry;

// Cloudflare Worker runtime (WASM): the `#[event(fetch)]` entry point + edge
// proxy. Only compiled for wasm32.
#[cfg(target_arch = "wasm32")]
pub mod worker_rt;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use axum::{
    middleware::from_fn_with_state,
    routing::{any, get},
    Router,
};

#[cfg(not(target_arch = "wasm32"))]
use crate::{
    config::AppConfig,
    policy::PolicyEngine,
    telemetry::{metrics_middleware, MetricsRegistry},
};

/// Shared application state threaded through the router and middleware.
///
/// `AppState` is intentionally cheap to clone (everything behind an `Arc`) so it
/// can be attached to multiple Tower layers and handlers.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub metrics: Arc<MetricsRegistry>,
    /// The Nova Guard policy engine. Always present; when guardrails are disabled
    /// or no policies are loaded it evaluates to a no-op that allows every request.
    pub policy: Arc<PolicyEngine>,
    /// Optional provider of platform live cost/rate state (for `cost_cap`/
    /// `rate_limit`). `None` when platform-managed Nova Guard isn't configured.
    pub live: Option<Arc<crate::policy::remote::RemoteLiveState>>,
    /// Optional reporter of BLOCKED usage events (from the guard middleware).
    /// `None` when platform-managed Nova Guard isn't configured. ALLOWED events
    /// are reported by the telemetry usage exporter instead.
    pub usage: Option<crate::policy::usage::UsageReporter>,
    /// Optional client for the platform's atomic admission API, used by
    /// strict-mode `cost_cap` policies so a cap holds across replicas instead of
    /// being enforced once per process. `None` when the bridge isn't configured.
    pub admission: Option<Arc<crate::policy::admission::AdmissionClient>>,
    /// Shared-gateway tenancy. `Some` only when `NOVEUM_GUARD_TENANCY=shared`:
    /// every `/v1/*` caller is then authenticated and its project +
    /// organization derived from the credential, and the four fields above are
    /// *not* used for enforcement (each tenant brings its own). `None` is
    /// dedicated mode — one process-wide project, exactly as before.
    pub tenancy: Option<Arc<crate::policy::middleware::SharedTenancy>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl AppState {
    pub fn new(
        config: Arc<AppConfig>,
        metrics: Arc<MetricsRegistry>,
        policy: Arc<PolicyEngine>,
        live: Option<Arc<crate::policy::remote::RemoteLiveState>>,
        usage: Option<crate::policy::usage::UsageReporter>,
        admission: Option<Arc<crate::policy::admission::AdmissionClient>>,
        tenancy: Option<Arc<crate::policy::middleware::SharedTenancy>>,
    ) -> Self {
        Self {
            config,
            metrics,
            policy,
            live,
            usage,
            admission,
            tenancy,
        }
    }
}

/// Build the gateway router with the full middleware stack.
///
/// Layer order (outermost first as applied by Axum, executed request-side
/// top-to-bottom):
///
/// 1. CORS (applied last in code → outermost)
/// 2. `metrics_middleware` — existing telemetry capture
/// 3. `policy::middleware::guard_middleware` — Nova Guard pre/post-call enforcement
///
/// The policy middleware is always wired but becomes a transparent pass-through
/// when the engine has no active policies, so enabling/disabling guardrails is a
/// runtime concern, not a routing concern.
///
/// In **shared** tenancy mode one more layer sits between telemetry and the
/// guard: `tenant_middleware` authenticates every `/v1/*` caller, derives its
/// project + organization from the credential, and replaces the process-wide
/// `GuardState` with that tenant's own for the rest of the request. It is not
/// wired at all in dedicated mode, so the dedicated request path is byte-for-
/// byte what it was.
#[cfg(not(target_arch = "wasm32"))]
pub fn build_router(state: AppState) -> Router {
    // Before the listener binds, not on the first proxied request.
    proxy::warm_http_clients();

    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
        .max_age(std::time::Duration::from_secs(3600));

    let mut router = Router::new()
        .route("/health", get(handlers::health_check))
        .route("/v1/*path", any(handlers::proxy_request))
        // Nova Guard policy enforcement runs closest to the handler so it sees the
        // final request and can short-circuit before the upstream provider call.
        .layer(from_fn_with_state(
            policy::middleware::GuardState {
                engine: state.policy.clone(),
                live: state.live.clone(),
                usage: state.usage.clone(),
                pending: Arc::new(policy::remote::PendingSpend::new()),
                admission: state.admission.clone(),
            },
            policy::middleware::guard_middleware,
        ));

    // Shared gateway: authenticate the caller and derive its tenant BEFORE the
    // guard layer, so the guard never sees a request whose tenant is unknown.
    if let Some(tenancy) = state.tenancy.clone() {
        router = router.layer(from_fn_with_state(
            tenancy,
            policy::middleware::tenant_middleware,
        ));
    }

    router
        // Telemetry capture wraps the policy layer so blocked requests are still
        // measured.
        .layer(from_fn_with_state(
            state.metrics.clone(),
            metrics_middleware,
        ))
        .with_state(state.config.clone())
        .layer(cors)
}
