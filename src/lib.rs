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

pub mod config;
pub mod context;
pub mod error;
pub mod handlers;
pub mod policy;
pub mod providers;
pub mod proxy;
pub mod telemetry;

use std::sync::Arc;

use axum::{
    middleware::from_fn_with_state,
    routing::{any, get},
    Router,
};

use crate::{
    config::AppConfig,
    policy::PolicyEngine,
    telemetry::{metrics_middleware, MetricsRegistry},
};

/// Shared application state threaded through the router and middleware.
///
/// `AppState` is intentionally cheap to clone (everything behind an `Arc`) so it
/// can be attached to multiple Tower layers and handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub metrics: Arc<MetricsRegistry>,
    /// The Nova Guard policy engine. Always present; when guardrails are disabled
    /// or no policies are loaded it evaluates to a no-op that allows every request.
    pub policy: Arc<PolicyEngine>,
}

impl AppState {
    pub fn new(
        config: Arc<AppConfig>,
        metrics: Arc<MetricsRegistry>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self {
            config,
            metrics,
            policy,
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
pub fn build_router(state: AppState) -> Router {
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
        .max_age(std::time::Duration::from_secs(3600));

    Router::new()
        .route("/health", get(handlers::health_check))
        .route("/v1/*path", any(handlers::proxy_request))
        // Nova Guard policy enforcement runs closest to the handler so it sees the
        // final request and can short-circuit before the upstream provider call.
        .layer(from_fn_with_state(
            state.policy.clone(),
            policy::middleware::guard_middleware,
        ))
        // Telemetry capture wraps the policy layer so blocked requests are still
        // measured.
        .layer(from_fn_with_state(
            state.metrics.clone(),
            metrics_middleware,
        ))
        .with_state(state.config.clone())
        .layer(cors)
}
