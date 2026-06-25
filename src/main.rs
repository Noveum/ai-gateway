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
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use noveum_ai_gateway::{
    build_router,
    config::{AppConfig, TelemetryConfig},
    control_plane::{spawn_policy_refresh, ControlPlaneClient, ControlPlaneConfig},
    policy::PolicyEngine,
    telemetry::{exporters::NoveumTraceExporter, ConsolePlugin, MetricsRegistry},
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

    // Hosted policy distribution: when the Noveum control plane is configured
    // (`NOVEUM_ENDPOINT` + `NOVEUM_API_KEY`) and a project id is set, poll the
    // control plane for the project's policy bundle and hot-swap it into the
    // engine. This is what makes hosted policies (and, server-side, strict budget
    // reservation) active; without it the gateway runs local/standalone policies.
    if let Some(cp_config) = ControlPlaneConfig::from_env() {
        match std::env::var("NOVEUM_PROJECT_ID")
            .or_else(|_| std::env::var("NOVEUM_PROJECT"))
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(project_id) => {
                let interval = std::env::var("NOVEUM_POLICY_REFRESH_SECS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|s| *s > 0)
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_secs(30));
                spawn_policy_refresh(
                    policy_engine.clone(),
                    ControlPlaneClient::new(cp_config),
                    project_id.clone(),
                    interval,
                );
                info!(
                    "Nova Guard: hosted policy distribution enabled (project={}, every {}s)",
                    project_id,
                    interval.as_secs()
                );
            }
            None => {
                warn!(
                    "NOVEUM_ENDPOINT/NOVEUM_API_KEY are set but NOVEUM_PROJECT_ID is missing; \
                     hosted policy distribution is disabled (using local policies only)"
                );
            }
        }
    }

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
