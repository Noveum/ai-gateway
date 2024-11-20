use axum::{
    middleware, routing::{any, get}, Router
};
use interceptors::key_fetcher;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, debug};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use std::time::Duration;

mod config;
mod error;
mod handlers;
mod providers;
mod proxy;
mod interceptors;

use crate::config::AppConfig;

#[tokio::main]
async fn main() {
    // Initialize tracing
    info!("Initializing tracing system");
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    // Load configuration
    info!("Loading application configuration");
    let config = Arc::new(AppConfig::new());
    debug!("Configuration loaded: port={}, host={}", config.port, config.host);

    // Optimize tokio runtime
    info!("Configuring tokio runtime with {} worker threads", config.worker_threads);
    std::env::set_var("TOKIO_WORKER_THREADS", config.worker_threads.to_string());
    std::env::set_var("TOKIO_THREAD_STACK_SIZE", (2 * 1024 * 1024).to_string());

    // Setup CORS
    debug!("Setting up CORS layer with 1-hour max age");
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .max_age(Duration::from_secs(3600));

    // Create router with optimized settings
    let app = Router::new()
        .route("/health", get(handlers::health_check))
        .route("/v1/*path", any(handlers::proxy_request))
        .route_layer(middleware::map_request_with_state(config.clone(), key_fetcher))
        .with_state(config.clone())
        .layer(cors)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

    // Start server with optimized TCP settings
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Setting up TCP listener with non-blocking mode");
    let tcp_listener = std::net::TcpListener::bind(addr).expect("Failed to bind address");
    tcp_listener.set_nonblocking(true).expect("Failed to set non-blocking");
    
    debug!("Converting to tokio TCP listener");
    let listener = tokio::net::TcpListener::from_std(tcp_listener)
        .expect("Failed to create Tokio TCP listener");

    info!(
        "AI Gateway listening on {}:{} with {} worker threads",
        config.host, config.port, config.worker_threads
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            error!("Server error: {}", e);
            std::process::exit(1);
        });
}

async fn shutdown_signal() {
    info!("Registering shutdown signal handler");
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C signal handler")
    };

    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("Shutdown signal received, starting graceful shutdown");
}
