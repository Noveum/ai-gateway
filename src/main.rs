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
    telemetry::{ConsolePlugin, MetricsRegistry},
    AppState,
};

fn main() {
    // Load `.env` before tracing so `RUST_LOG` and the runtime settings are
    // available before Tokio creates any worker threads.
    dotenv::dotenv().ok();
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    info!("Loading application configuration");
    let config = Arc::new(AppConfig::new());
    debug!(
        "Configuration loaded: port={}, host={}, worker_threads={}",
        config.port, config.host, config.worker_threads
    );

    let runtime = build_runtime(config.worker_threads).unwrap_or_else(|error| {
        eprintln!("Failed to build Tokio runtime: {error}");
        std::process::exit(1);
    });
    runtime.block_on(run(config));
}

fn build_runtime(worker_threads: usize) -> std::io::Result<tokio::runtime::Runtime> {
    if worker_threads == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "WORKER_THREADS must be greater than zero",
        ));
    }

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_stack_size(2 * 1024 * 1024)
        .enable_all()
        .build()
}

async fn run(config: Arc<AppConfig>) {
    print_banner().await;
    info!(
        "Tokio runtime started with {} worker threads",
        config.worker_threads
    );

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

    // Nova Guard policy engine. Platform-managed Nova Guard runs in one of two
    // explicit, mutually exclusive deployment modes (`NOVEUM_GUARD_TENANCY`):
    //
    // * DEDICATED (`NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID`) — one
    //   process-wide project. Policies are fetched once at startup and live
    //   cost/rate state is queried per request, all for that one project. This
    //   is also what an unset `NOVEUM_GUARD_TENANCY` means, so existing
    //   deployments are untouched.
    // * SHARED (`NOVEUM_GUARD_TENANCY=shared`, no process-wide project or key) —
    //   every `/v1/*` caller presents its own Noveum key, and the project +
    //   organization to enforce against are derived from it server-side. This
    //   is what a multi-tenant deployment such as `gateway.noveum.ai` requires.
    //
    // With neither configured, policies load from the local source
    // (`NOVEUM_GUARD_POLICIES_FILE` / inline `NOVEUM_GUARD_POLICIES`).
    //
    // Nothing here degrades an *explicitly configured* guard into a silent
    // pass-through: a broken or half-applied configuration, and a platform
    // bridge whose first policy fetch fails, both abort startup. A gateway that
    // looks healthy while enforcing nothing is the worst possible outcome, so
    // the only way to serve unguarded traffic under a configured bridge is the
    // deliberate `NOVEUM_GUARD_ALLOW_UNGUARDED_START` escape hatch below.
    info!("Initializing Nova Guard policy engine");
    use noveum_ai_gateway::policy::engine::EngineOptions;
    use noveum_ai_gateway::policy::remote::RemoteLiveState;
    use noveum_ai_gateway::policy::usage::UsageReporter;
    use noveum_ai_gateway::telemetry::NovaGuardUsagePlugin;

    /// Abort startup on a Nova Guard configuration the gateway must not paper
    /// over. Logs at ERROR (so it lands in whatever collects stderr) and exits
    /// non-zero, which makes a Kubernetes rollout fail visibly instead of
    /// bringing up replicas that enforce nothing.
    fn fatal_guard_config(error: &str) -> ! {
        tracing::error!(error, "Nova Guard: refusing to start");
        eprintln!("FATAL: Nova Guard configuration error: {error}");
        std::process::exit(1);
    }

    let allow_unguarded_start =
        std::env::var(noveum_ai_gateway::policy::remote::ALLOW_UNGUARDED_START_VAR)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

    // Which deployment mode? A half-applied or contradictory configuration
    // (shared tenancy *and* a process-wide project id, `dedicated` with no
    // credentials, an unknown mode string) is an error here, not a precedence
    // rule resolved silently at runtime.
    let tenancy = noveum_ai_gateway::policy::remote::GuardTenancy::from_env()
        .unwrap_or_else(|e| fatal_guard_config(&e.to_string()));

    // A local bundle in shared mode would be loaded and never enforced (each
    // tenant is evaluated against its own policy set), and a local
    // `cost_cap`/`rate_limit` would be one counter shared by every tenant.
    if let Some(e) = noveum_ai_gateway::policy::remote::shared_mode_local_bundle_conflict(
        matches!(
            tenancy,
            Some(noveum_ai_gateway::policy::remote::GuardTenancy::Shared(_))
        ),
        std::env::var("NOVEUM_GUARD_POLICIES_FILE").ok().as_deref(),
        std::env::var("NOVEUM_GUARD_POLICIES").ok().as_deref(),
    ) {
        fatal_guard_config(&e);
    }

    let remote_cfg = tenancy.as_ref().and_then(|t| t.dedicated()).cloned();
    let shared_cfg = match &tenancy {
        Some(noveum_ai_gateway::policy::remote::GuardTenancy::Shared(c)) => Some(c.clone()),
        _ => None,
    };

    // Client for the platform's atomic admission API (strict-mode cost caps).
    // Validated against the bridge configuration: `NOVEUM_GUARD_COST_ENFORCEMENT=strict`
    // with no bridge is a hard error, not a silent downgrade to per-replica
    // (i.e. replica-count-multiplied) enforcement.
    //
    // Shared mode has no process-wide client: admission reserves against ONE
    // project's counter, so each tenant gets its own (built by `SharedTenancy`,
    // which validates the same variable).
    let admission = if shared_cfg.is_some() {
        None
    } else {
        noveum_ai_gateway::policy::admission::AdmissionClient::from_env(remote_cfg.as_ref())
            .unwrap_or_else(|e| fatal_guard_config(&e.to_string()))
            .map(Arc::new)
    };
    if admission.is_some() {
        info!(
            "Nova Guard: platform atomic admission available (used by cost caps in strict \
             enforcement mode; advisory caps keep the in-process pending ledger)"
        );
    }

    // Shared gateway: build the tenancy layer. Nothing is fetched at startup —
    // there is no tenant yet — but a misconfiguration still aborts here.
    let tenancy_layer = shared_cfg.map(|cfg| {
        let base_url = cfg.base_url.clone();
        let ttl = cfg.resolution_ttl;
        let max = cfg.max_tenants;
        let shared = noveum_ai_gateway::policy::middleware::SharedTenancy::from_env(
            cfg,
            allow_unguarded_start,
        )
        .unwrap_or_else(|e| fatal_guard_config(&e.to_string()));
        info!(
            api = %base_url, resolution_ttl_secs = ttl.as_secs(), max_tenants = max,
            credential_header = noveum_ai_gateway::policy::remote::TENANT_CREDENTIAL_HEADER,
            "Nova Guard: SHARED gateway — every /v1/* caller is authenticated and its project + \
             organization are derived from its own Noveum key; policies, counters, reservations \
             and usage are keyed by that derived tenant"
        );
        Arc::new(shared)
    });

    let (policy_engine, live, usage) = if tenancy_layer.is_some() {
        // The process-wide engine is a deliberately empty no-op in shared mode:
        // every guarded request is evaluated against the engine of the tenant
        // derived from its credential, injected by `tenant_middleware`. There is
        // likewise no process-wide live state or usage reporter — one of either
        // would be a cross-tenant counter.
        (
            Arc::new(PolicyEngine::from_bundle(
                &noveum_ai_gateway::policy::PolicyBundle::default(),
                EngineOptions::default(),
            )),
            None,
            None,
        )
    } else {
        match remote_cfg {
            Some(cfg) => {
                info!(
                    project = %cfg.project_id, api = %cfg.base_url,
                    "Nova Guard: fetching policies from the Noveum platform"
                );
                // A live-state backend (the platform bridge) is wired here, so the
                // engine honors `failClosed` (a `/state` outage blocks).
                let mut opts = EngineOptions::from_env();
                opts.live_state_backed = true;
                // Aborts startup if the first fetch fails, so no replica ever
                // serves `/v1/*` believing a policy set is loaded when none is.
                let (engine, etag) = noveum_ai_gateway::policy::remote::bootstrap_engine(
                    &cfg,
                    opts.clone(),
                    allow_unguarded_start,
                )
                .await
                .unwrap_or_else(|e| fatal_guard_config(&e));
                let engine = Arc::new(engine);
                // Background poller: refresh policies from `/effective` ~60s and
                // hot-swap the engine (self-heals if the startup fetch failed).
                noveum_ai_gateway::policy::remote::spawn_policy_poller(
                    cfg.clone(),
                    engine.clone(),
                    etag,
                );
                // Spawn the usage reporter and register the ALLOWED exporter. BLOCKED
                // events are reported from the guard middleware via the same reporter.
                //
                // The exporter is gated on the admission client: while a strict
                // `cost_cap` is active, each request's *reservation settlement* is
                // its metered record, and reporting it here as well would count the
                // same call twice (halving every cap).
                let reporter = UsageReporter::spawn(cfg.clone());
                let mut exporter = NovaGuardUsagePlugin::new(reporter.clone());
                if let Some(admission) = &admission {
                    exporter = exporter.metered_by_admission(engine.clone(), admission.clone());
                }
                metrics_registry.register_exporter(Box::new(exporter)).await;
                (
                    engine,
                    Some(Arc::new(RemoteLiveState::new(cfg))),
                    Some(reporter),
                )
            }
            None => {
                // No platform bridge. A local bundle is optional, but a configured
                // one that fails to load is fatal rather than silently ignored.
                let engine = PolicyEngine::from_env()
                    .await
                    .unwrap_or_else(|e| fatal_guard_config(&e));
                (Arc::new(engine), None, None)
            }
        }
    };
    if tenancy_layer.is_some() {
        info!("Nova Guard: policies are per-tenant in shared mode; none are loaded process-wide");
    } else {
        info!(
            "Nova Guard: {} active policies ({}{})",
            policy_engine.active_policy_count(),
            if policy_engine.is_enabled() {
                "enabled"
            } else {
                "disabled (pass-through)"
            },
            if live.is_some() {
                ", platform live-state + usage reporting"
            } else {
                ""
            }
        );
    }

    // Kept out of `AppState` so the shutdown path below can flush whatever is
    // still queued once the server stops accepting requests.
    let usage_at_shutdown = usage.clone();

    // Build the router with the full middleware stack.
    info!("Registering request handlers and API routes");
    let state = AppState::new(
        config.clone(),
        metrics_registry.clone(),
        policy_engine.clone(),
        live,
        usage,
        admission,
        tenancy_layer,
    );
    let app = build_router(state);

    // Start server with optimized TCP settings
    info!("Setting up TCP listener with non-blocking mode");
    let tcp_listener = std::net::TcpListener::bind(config.bind_target())
        .expect("Failed to bind configured HOST and PORT");
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

    // In-flight requests have finished; nothing new can be enqueued. Push the
    // remaining usage records to the platform before the process exits, so a
    // rolling restart doesn't silently lose billable events. Bounded by
    // `SHUTDOWN_FLUSH_BUDGET`: an unresponsive platform delays exit by at most
    // that long, it can never hang the shutdown.
    flush_usage_on_shutdown(usage_at_shutdown).await;
}

/// Drain the Nova Guard usage queue on graceful shutdown, under a fixed budget.
async fn flush_usage_on_shutdown(
    reporter: Option<noveum_ai_gateway::policy::usage::UsageReporter>,
) {
    use noveum_ai_gateway::policy::usage::SHUTDOWN_FLUSH_BUDGET;

    let Some(reporter) = reporter else {
        return; // no platform bridge configured → nothing to report
    };
    let queued = reporter.pending_events();
    if queued > 0 {
        info!(
            queued,
            timeout_secs = SHUTDOWN_FLUSH_BUDGET.as_secs(),
            "Nova Guard: flushing queued usage events before exit"
        );
    }
    let outcome = reporter.shutdown(SHUTDOWN_FLUSH_BUDGET).await;
    if outcome.timed_out || outcome.pending > 0 || outcome.failed > 0 {
        error!(
            delivered = outcome.delivered,
            failed = outcome.failed,
            pending = outcome.pending,
            timed_out = outcome.timed_out,
            dropped_total = reporter.dropped_events(),
            "Nova Guard: usage flush incomplete at shutdown; some events were not reported"
        );
    } else if outcome.delivered > 0 {
        info!(
            delivered = outcome.delivered,
            dropped_total = reporter.dropped_events(),
            "Nova Guard: usage flushed at shutdown"
        );
    }
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

    let banner = format!(
        r#"

     _   _
    | \ | | _____   _____ _   _ _ __ ___
    |  \| |/ _ \ \ / / _ \ | | | '_ ` _ \
    | |\  | (_) \ V /  __/ |_| | | | | | |
    |_| \_|\___/ \_/ \___|\__,_|_| |_| |_|

             AI Gateway v{}
    ========================================
    "#,
        env!("CARGO_PKG_VERSION")
    );
    println!("{}", banner.bright_cyan());

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

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_uses_the_configured_worker_thread_count() {
        let runtime = super::build_runtime(3).expect("test runtime should build");
        assert_eq!(runtime.metrics().num_workers(), 3);
    }
}
