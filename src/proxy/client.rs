use crate::config::AppConfig;
use once_cell::sync::Lazy;
use std::time::Duration;
use tracing::info;

pub fn create_client(config: &AppConfig) -> reqwest::Client {
    info!("Creating HTTP client with optimized settings");

    reqwest::Client::builder()
        .pool_max_idle_per_host(config.max_connections)
        .pool_idle_timeout(Duration::from_secs(30))
        .http2_prior_knowledge()
        .http2_keep_alive_interval(Duration::from_secs(5))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_adaptive_window(true)
        .tcp_keepalive(Duration::from_secs(5))
        .tcp_nodelay(true)
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(32)
        .gzip(true)
        .brotli(true)
        .build()
        .expect("Failed to create HTTP client")
}

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let config = AppConfig::new();
    create_client(&config)
});
