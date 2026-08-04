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

/// Fallback client that negotiates the HTTP version normally (ALPN h2 or
/// HTTP/1.1 over TLS; HTTP/1.1 for cleartext). [`CLIENT`] forces HTTP/2 prior
/// knowledge, which cannot speak to an HTTP/1.1-only server at all — a plain
/// `http://` upstream (local mock provider, self-hosted gateway) or a custom
/// HTTPS endpoint behind an HTTP/1.1-only proxy would fail with an opaque h2
/// error before the request ever reached the provider.
pub static NEGOTIATING_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
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
        .expect("Failed to create negotiating HTTP client")
});

/// First-party provider API hosts that are known to accept direct HTTP/2 —
/// only these get the prior-knowledge [`CLIENT`]. Custom endpoints (an
/// `OPENAI_BASE_URL` override, a self-hosted compatible gateway) cannot be
/// assumed to speak h2 directly and go through [`NEGOTIATING_CLIENT`].
const H2_PROVIDER_HOSTS: &[&str] = &[
    "api.openai.com",
    "api.groq.com",
    "api.together.xyz",
    "api.fireworks.ai",
    "api.mistral.ai",
    "api.deepseek.com",
    "api.x.ai",
    "openrouter.ai",
    "api.perplexity.ai",
    "generativelanguage.googleapis.com",
    "api.cohere.ai",
    "api.anthropic.com",
];

/// Extract the host from an absolute URL (no scheme/port/path).
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    authority.rsplit('@').next()?.split(':').next()
}

/// Pick the right client for an upstream URL: h2-prior-knowledge for the known
/// first-party provider hosts, a normally-negotiating client for everything
/// else (cleartext mocks, custom/overridden endpoints, AWS SigV4 hosts).
pub fn client_for_url(url: &str) -> &'static reqwest::Client {
    let is_known_h2 = url.starts_with("https://")
        && url_host(url)
            .is_some_and(|h| H2_PROVIDER_HOSTS.contains(&h) || h.ends_with(".amazonaws.com"));
    if is_known_h2 {
        &CLIENT
    } else {
        &NEGOTIATING_CLIENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_provider_hosts_get_prior_knowledge_client() {
        assert!(std::ptr::eq(
            client_for_url("https://api.openai.com/v1/chat/completions"),
            &*CLIENT
        ));
        assert!(std::ptr::eq(
            client_for_url("https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse"),
            &*CLIENT
        ));
    }

    #[test]
    fn custom_and_cleartext_endpoints_negotiate() {
        // Cleartext mock provider.
        assert!(std::ptr::eq(
            client_for_url("http://127.0.0.1:8788/v1/chat/completions"),
            &*NEGOTIATING_CLIENT
        ));
        // Custom HTTPS endpoint (e.g. OPENAI_BASE_URL override) — may be
        // HTTP/1.1-only, so it must not get the prior-knowledge client.
        assert!(std::ptr::eq(
            client_for_url("https://my-gateway.corp.example/v1/chat/completions"),
            &*NEGOTIATING_CLIENT
        ));
    }

    #[test]
    fn url_host_parses_ports_paths_and_userinfo() {
        assert_eq!(
            url_host("https://api.openai.com/v1/x"),
            Some("api.openai.com")
        );
        assert_eq!(url_host("http://127.0.0.1:8788/v1"), Some("127.0.0.1"));
        assert_eq!(
            url_host("https://u:p@host.example:443/x"),
            Some("host.example")
        );
        assert_eq!(url_host("not-a-url"), None);
    }
}
