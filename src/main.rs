use axum::{
    routing::{any, get},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod error;
mod handlers;
mod providers;
mod proxy;

use crate::config::AppConfig;

#[tokio::main]
async fn main() {
    // Initialize tracing with more detailed format
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,tower_http=debug,axum::rejection=trace".into()),
        ))
        .with(
            tracing_subscriber::fmt::layer()
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_thread_names(true),
        )
        .init();

    // Load configuration
    let config = Arc::new(AppConfig::new());

    info!(
        host = %config.port,
        port = %config.port,
        "Starting server with configuration"
    );

    // Setup CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    info!("CORS configuration: allowing all origins, methods, and headers");

    // Create router
    let app = Router::new()
        .route("/health", get(handlers::health_check))
        .route("/v1/*path", any(handlers::proxy_request))
        .with_state(config.clone())
        .layer(cors);

    info!("Router configured with health check and proxy endpoints");

    // Start server
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    info!(
        address = %addr,
        "Starting server"
    );

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            info!("Server successfully bound to address");
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "Server error occurred");
                std::process::exit(1);
            }
        }
        Err(e) => {
            error!(error = %e, "Failed to bind server to address");
            std::process::exit(1);
        }
    }
}
