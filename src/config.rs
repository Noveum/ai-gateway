use num_cpus;
use std::env;
use tracing::debug;
use tracing::info;

pub struct AppConfig {
    pub port: u16,
    pub host: String,
    pub worker_threads: usize,
    pub max_connections: usize,
    pub tcp_keepalive_interval: u64,
    pub tcp_nodelay: bool,
    pub buffer_size: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl AppConfig {
    pub fn new() -> Self {
        info!("Loading environment configuration");
        dotenv::dotenv().ok();

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
            tcp_keepalive_interval: env::var("TCP_KEEPALIVE_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            tcp_nodelay: env::var("TCP_NODELAY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            buffer_size: env::var("BUFFER_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8 * 1024), // 8KB default
        };

        info!(
            "Configuration loaded: port={}, host={}",
            config.port, config.host
        );
        debug!(
            "Advanced settings: workers={}, max_conn={}, buffer_size={}",
            config.worker_threads, config.max_connections, config.buffer_size
        );

        config
    }
}

#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    pub debug_mode: bool,
    pub elasticsearch_enabled: bool,
    pub cloudwatch_enabled: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            debug_mode: std::env::var("DEBUG_METRICS")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
            elasticsearch_enabled: std::env::var("ENABLE_ELASTICSEARCH")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
            cloudwatch_enabled: std::env::var("ENABLE_CLOUDWATCH")
                .map(|v| v.parse().unwrap_or(false))
                .unwrap_or(false),
        }
    }
}
