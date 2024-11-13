use once_cell::sync::Lazy;
use std::time::Duration;

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(60))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("Failed to create HTTP client")
});
