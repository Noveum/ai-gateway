//! Runtime configuration loaded from the environment.
//!
//! [`AppConfig`] holds server/runtime settings (port, host, worker threads, HTTP
//! connection pool size) and [`TelemetryConfig`] gates the optional telemetry
//! exporters. All values are read once at startup via [`AppConfig::new`].

use num_cpus;
use std::env;
use tracing::debug;
use tracing::info;

/// Server and runtime configuration, sourced from environment variables.
pub struct AppConfig {
    /// TCP port to listen on (`PORT`, default `3000`).
    pub port: u16,
    /// Bind address (`HOST`, default `127.0.0.1`).
    pub host: String,
    /// Tokio worker thread count (`WORKER_THREADS`, default derived from cores).
    pub worker_threads: usize,
    /// Max idle HTTP connections kept per upstream host (`MAX_CONNECTIONS`).
    pub max_connections: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AppConfig {
    pub fn new() -> Self {
        info!("Loading environment configuration");
        dotenvy::dotenv().ok();

        // Optimize thread count based on CPU cores
        let cpu_count = num_cpus::get();
        debug!("Detected {} CPU cores", cpu_count);

        let default_workers = if cpu_count <= 4 {
            cpu_count * 2
        } else {
            cpu_count + 4
        };
        debug!("Calculated default worker threads: {}", default_workers);

        let config = Self {
            port: env::var("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .expect("PORT must be a number"),
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            worker_threads: env::var("WORKER_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default_workers),
            max_connections: env::var("MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10_000),
        };

        info!(
            "Configuration loaded: port={}, host={}",
            config.port, config.host
        );
        debug!(
            "Advanced settings: workers={}, max_conn={}",
            config.worker_threads, config.max_connections
        );

        config
    }

    /// Host and port passed to the native TCP listener.
    ///
    /// Keeping this in the configuration type prevents the startup path from
    /// silently replacing the operator's `HOST` value with a wildcard bind.
    pub fn bind_target(&self) -> (&str, u16) {
        (&self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn bind_target_uses_the_configured_host_and_port() {
        let config = AppConfig {
            port: 43_210,
            host: "127.0.0.2".to_string(),
            worker_threads: 1,
            max_connections: 1,
        };

        assert_eq!(config.bind_target(), ("127.0.0.2", 43_210));
    }
}

/// Gates the optional telemetry exporters.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// When set (`DEBUG_METRICS=true`), register the console metrics exporter.
    pub debug_mode: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            debug_mode: std::env::var("DEBUG_METRICS")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
        }
    }
}
