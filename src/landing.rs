//! Shared, dependency-free HTML for the gateway's public root page.
//!
//! The native server and Cloudflare Worker serve the same content and security
//! headers so `/` remains useful without introducing a second web application
//! or an active-content supply chain.

/// Browser policy for the static landing page.
///
/// The page has no JavaScript, images, forms, network requests, or third-party
/// assets. Its only executable-adjacent content is the inline stylesheet.
pub const LANDING_CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

/// Short cache lifetime for an immutable-per-release page while still allowing
/// a newly deployed version number to become visible quickly.
pub const LANDING_CACHE_CONTROL: &str = "public, max-age=300";

/// Build the public gateway page for a named runtime.
///
/// Callers pass a compile-time runtime label (`native-server` or
/// `cloudflare-worker`). Escaping it here keeps the helper safe if a future
/// deployment obtains that label from configuration instead.
pub fn landing_page_html(runtime: &str) -> String {
    let runtime = escape_html(runtime);
    let version = env!("CARGO_PKG_VERSION");

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <meta name="description" content="Noveum AI Gateway — one OpenAI-compatible endpoint for multiple AI providers.">
  <title>Noveum AI Gateway</title>
  <style>
    :root {{ color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; color: #eef2ff; background: radial-gradient(circle at 15% 10%, #23346b 0, transparent 34rem), #090d1a; }}
    main {{ width: min(960px, calc(100% - 32px)); margin: 0 auto; padding: 72px 0 48px; }}
    .status {{ display: inline-flex; align-items: center; gap: 9px; padding: 7px 11px; border: 1px solid #3d5189; border-radius: 999px; background: #111a35cc; color: #cbd5ff; font-size: 13px; }}
    .dot {{ width: 8px; height: 8px; border-radius: 50%; background: #50e3a4; box-shadow: 0 0 16px #50e3a4; }}
    h1 {{ max-width: 720px; margin: 28px 0 14px; font-size: clamp(42px, 8vw, 74px); line-height: .98; letter-spacing: -.055em; }}
    .lead {{ max-width: 650px; margin: 0; color: #aeb9db; font-size: clamp(17px, 2vw, 21px); line-height: 1.6; }}
    .grid {{ display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 16px; margin-top: 42px; }}
    .card {{ min-width: 0; min-height: 174px; padding: 22px; border: 1px solid #26345f; border-radius: 18px; background: linear-gradient(145deg, #111831e6, #0c1124e6); box-shadow: 0 18px 60px #02040c66; }}
    .card.quick {{ grid-column: 1 / -1; min-height: 0; }}
    .card h2 {{ margin: 0 0 10px; font-size: 16px; letter-spacing: -.01em; }}
    .card p {{ margin: 0; color: #9ca9cb; line-height: 1.55; }}
    code {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; color: #cdd7ff; }}
    pre {{ min-width: 0; max-width: 100%; overflow-wrap: anywhere; white-space: pre-wrap; margin: 14px 0 0; padding: 15px; border: 1px solid #27365f; border-radius: 12px; background: #060a15; color: #c9d5ff; font-size: 12px; line-height: 1.55; }}
    nav {{ display: flex; flex-wrap: wrap; gap: 10px; margin-top: 28px; }}
    a {{ padding: 10px 14px; border: 1px solid #34477c; border-radius: 10px; color: #d6ddff; text-decoration: none; background: #111a35; }}
    a:hover, a:focus-visible {{ border-color: #7c95e8; background: #18244b; outline: none; }}
    footer {{ margin-top: 38px; color: #7380a5; font-size: 13px; }}
    @media (max-width: 700px) {{
      main {{ padding-top: 40px; }}
      .grid {{ grid-template-columns: minmax(0, 1fr); }}
      .card.quick {{ grid-column: auto; }}
    }}
  </style>
</head>
<body>
  <main>
    <div class="status"><span class="dot" aria-hidden="true"></span>Gateway online</div>
    <h1>One gateway.<br>Multiple AI providers.</h1>
    <p class="lead">Noveum AI Gateway provides an OpenAI-compatible request surface with provider routing, streaming, telemetry, and optional Nova Guard enforcement.</p>

    <section class="grid" aria-label="Gateway details">
      <article class="card">
        <h2>API endpoint</h2>
        <p>Send OpenAI-compatible chat requests to <code>/v1/chat/completions</code>. Select the upstream with the <code>x-provider</code> header.</p>
      </article>
      <article class="card">
        <h2>Release</h2>
        <p>Version <code>{version}</code><br>Runtime <code>{runtime}</code><br>Status <code>/health</code></p>
      </article>
      <article class="card quick">
        <h2>Quick start</h2>
        <pre>curl "$GATEWAY_URL/v1/chat/completions" \
  -H "Authorization: Bearer $PROVIDER_API_KEY" \
  -H "x-provider: openai" \
  -H "content-type: application/json" \
  -d '{{"model":"gpt-4o-mini","max_tokens":32,"messages":[{{"role":"user","content":"Hello"}}]}}'</pre>
      </article>
    </section>

    <nav aria-label="Project links">
      <a href="/health">Health</a>
      <a href="https://docs.rs/noveum-ai-gateway/">Rust documentation</a>
      <a href="https://github.com/Noveum/ai-gateway">Source and guides</a>
      <a href="https://noveum.ai">Noveum</a>
    </nav>
    <footer>Noveum AI Gateway · API responses are served under <code>/v1/*</code>.</footer>
  </main>
</body>
</html>"#
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::{landing_page_html, LANDING_CONTENT_SECURITY_POLICY};

    #[test]
    fn landing_page_identifies_the_release_and_stable_gateway_routes() {
        let html = landing_page_html("cloudflare-worker");

        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains(env!("CARGO_PKG_VERSION")));
        assert!(html.contains("cloudflare-worker"));
        assert!(html.contains("/health"));
        assert!(html.contains("/v1/chat/completions"));
        assert!(html.contains("https://docs.rs/noveum-ai-gateway/"));
        assert!(html.contains("https://github.com/Noveum/ai-gateway"));
        assert!(html.contains("\"model\":\"gpt-4o-mini\""));
        assert!(html.contains("\"max_tokens\":32"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<form"));
    }

    #[test]
    fn landing_page_escapes_the_runtime_label() {
        let html = landing_page_html("edge<script>alert(1)</script>");

        assert!(html.contains("edge&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("edge<script>"));
    }

    #[test]
    fn landing_page_security_policy_disallows_active_or_embedded_content() {
        assert_eq!(
            LANDING_CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
        );
    }
}
