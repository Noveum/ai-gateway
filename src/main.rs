//! Noveum AI Gateway — binary entry point.
//!
//! This is a thin bootstrap: it initializes tracing, loads configuration, builds
//! the telemetry registry and the Nova Guard policy engine, constructs the router
//! via [`noveum_ai_gateway::build_router`], and serves it with graceful shutdown.
//!
//! All request-handling logic lives in the library crate so it can be tested
//! without binding a port.

use std::{sync::Arc, time::Duration};

use colored::*;
use tokio::signal;
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use noveum_ai_gateway::{
    build_router,
    config::{AppConfig, TelemetryConfig},
    policy::PolicyEngine,
    telemetry::{
        exporters::NoveumTraceExporter, plugins::elasticsearch::ElasticsearchPlugin, ConsolePlugin,
        MetricsRegistry,
    },
    AppState,
};

#[tokio::main]
async fn main() {
    print_banner().await;

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
    debug!(
        "Configuration loaded: port={}, host={}, worker_threads={}",
        config.port, config.host, config.worker_threads
    );

    // Optimize tokio runtime
    info!(
        "Configuring tokio runtime with {} worker threads",
        config.worker_threads
    );
    std::env::set_var("TOKIO_WORKER_THREADS", config.worker_threads.to_string());
    std::env::set_var("TOKIO_THREAD_STACK_SIZE", (2 * 1024 * 1024).to_string());

    // Telemetry registry + exporters
    let telemetry_config = TelemetryConfig::default();
    debug!(
        "Telemetry configuration: debug_mode={}",
        telemetry_config.debug_mode
    );
    let metrics_registry = Arc::new(MetricsRegistry::new(telemetry_config.debug_mode));

    if telemetry_config.debug_mode {
        debug!("Registering Console plugin for metrics");
        metrics_registry
            .register_exporter(Box::new(ConsolePlugin::new()))
            .await;
    }

    if telemetry_config.elasticsearch_enabled {
        debug!("Registering Elasticsearch exporter");
        let elasticsearch_url = std::env::var("ELASTICSEARCH_URL")
            .unwrap_or_else(|_| "http://localhost:9200".to_string());
        let elasticsearch_username = std::env::var("ELASTICSEARCH_USERNAME").ok();
        let elasticsearch_password = std::env::var("ELASTICSEARCH_PASSWORD").ok();
        let elasticsearch_index = std::env::var("ELASTICSEARCH_INDEX")
            .unwrap_or_else(|_| "ai-gateway-metrics".to_string());

        match ElasticsearchPlugin::new(
            elasticsearch_url,
            elasticsearch_username,
            elasticsearch_password,
            elasticsearch_index,
        ) {
            Ok(plugin) => {
                metrics_registry.register_exporter(Box::new(plugin)).await;
                info!("Elasticsearch exporter registered successfully");
            }
            Err(e) => {
                error!("Failed to initialize Elasticsearch exporter: {}", e);
            }
        }
    }

    // Noveum trace exporter — ships gateway traffic to the Noveum platform's
    // trace-ingest endpoint so it appears alongside SDK traffic.
    if let Some(exporter) = NoveumTraceExporter::from_env() {
        metrics_registry.register_exporter(Box::new(exporter)).await;
        info!("Noveum trace exporter registered");
    }

    // Nova Guard policy engine. Loads policies from the configured source
    // (local file and/or the Noveum control plane). On any load failure it
    // degrades to a transparent pass-through so the gateway never fails to boot.
    info!("Initializing Nova Guard policy engine");
    let policy_engine = Arc::new(PolicyEngine::from_env().await);
    info!(
        "Nova Guard: {} active policies ({})",
        policy_engine.active_policy_count(),
        if policy_engine.is_enabled() {
            "enabled"
        } else {
            "disabled (pass-through)"
        }
    );

    // Build the router with the full middleware stack.
    info!("Registering request handlers and API routes");
    let state = AppState::new(
        config.clone(),
        metrics_registry.clone(),
        policy_engine.clone(),
    );
    let app = build_router(state);

    // Start server with optimized TCP settings
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Setting up TCP listener with non-blocking mode");
    let tcp_listener = std::net::TcpListener::bind(addr).expect("Failed to bind address");
    tcp_listener
        .set_nonblocking(true)
        .expect("Failed to set non-blocking");

    let listener = tokio::net::TcpListener::from_std(tcp_listener)
        .expect("Failed to create Tokio TCP listener");

    println!(
        "{}",
        r#"
    ╔══════════════════════════════════════════════╗
    ║                                              ║
    ║  🌟 Noveum AI Gateway is now ONLINE! 🌟      ║
    ║                                              ║
    ╚══════════════════════════════════════════════╝"#
            .bright_green()
    );

    info!(
        "AI Gateway listening on {}:{} with {} worker threads",
        config.host, config.port, config.worker_threads
    );
    println!(
        "{}",
        format!("    🔗 Listening at http://{}:{}", config.host, config.port).bright_cyan()
    );
    println!(
        "{}",
        "    🔄 Press Ctrl+C to shutdown gracefully".bright_yellow()
    );
    println!();

    debug!("Starting server with graceful shutdown");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap_or_else(|e| {
        error!("Server error: {}", e);
        std::process::exit(1);
    });
}

async fn print_banner() {
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    print!("\n    Starting Noveum AI Gateway ");
    for frame in frames.iter().cycle().take(15) {
        print!("\r    Starting Noveum AI Gateway {}  ", frame.bright_cyan());
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    println!("\r    Starting Noveum AI Gateway ✓  \n");

    println!(
        "{}",
        r#"

     _   _
    | \ | | _____   _____ _   _ _ __ ___
    |  \| |/ _ \ \ / / _ \ | | | '_ ` _ \
    | |\  | (_) \ V /  __/ |_| | | | | | |
    |_| \_|\___/ \_/ \___|\__,_|_| |_| |_|

             AI Gateway v1.0.0
    ========================================
    "#
        .bright_cyan()
    );

    println!("{}", "🚀 Starting Noveum AI Gateway...".bright_green());
    println!(
        "{}",
        "📡 Your unified interface to multiple AI providers".bright_yellow()
    );
    println!(
        "{}\n",
        "========================================".bright_cyan()
    );
}

async fn shutdown_signal() {
    info!("Registering shutdown signal handler");
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C signal handler")
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            println!("{}", "    🛑 Noveum AI Gateway shutting down...".bright_yellow());
            info!("SIGINT (Ctrl+C) received, starting graceful shutdown");
        },
        _ = terminate => {
            println!("{}", "    🛑 Noveum AI Gateway shutting down...".bright_yellow());
            info!("SIGTERM received, starting graceful shutdown");
        },
    }
}
