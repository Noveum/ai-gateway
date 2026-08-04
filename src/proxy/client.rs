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
        .gzip(true)
        .brotli(true)
        .build()
        .expect("Failed to create HTTP client")
}

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let config = AppConfig::new();
    create_client(&config)
});

/// Fallback client for plain-`http://` upstreams. [`CLIENT`] forces HTTP/2
/// prior knowledge (all real provider APIs are HTTPS + h2), which cannot speak
/// to an HTTP/1.1 cleartext server at all — any `http://` upstream (a local
/// mock provider, a self-hosted gateway) would fail with an opaque h2 error.
/// This one negotiates normally.
pub static HTTP1_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let config = AppConfig::new();
    reqwest::Client::builder()
        .pool_max_idle_per_host(config.max_connections)
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(5))
        .tcp_nodelay(true)
        .use_rustls_tls()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(3))
        .gzip(true)
        .brotli(true)
        .build()
        .expect("Failed to create HTTP/1.1 fallback client")
});

/// Pick the right client for an upstream URL: h2-prior-knowledge for HTTPS
/// providers, a normally-negotiating client for cleartext `http://` upstreams.
pub fn client_for_url(url: &str) -> &'static reqwest::Client {
    if url.starts_with("http://") {
        &HTTP1_CLIENT
    } else {
        &CLIENT
    }
}
