//! Shared request-routing helpers used by BOTH the native server and the
//! Cloudflare Worker, so provider resolution and input flattening behave
//! identically on every deployment shape.

use serde_json::Value;

/// Where an OpenAI-compatible request should be forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRoute {
    /// Upstream base URL (no trailing slash).
    pub base_url: &'static str,
    /// Strip the leading `/v1` from the request path before appending it to
    /// `base_url` (for bases that already carry a version segment).
    pub strip_v1: bool,
}

/// Resolve the OpenAI-compatible upstream for an `x-provider` value.
///
/// Returns `None` for providers that need request/auth transforms beyond simple
/// pass-through (currently `anthropic` and `bedrock`), which the edge handles in
/// later phases. Mirrors the native `providers::create_provider` base URLs.
pub fn resolve_provider(name: &str) -> Option<ProviderRoute> {
    let (base_url, strip_v1) = match name.to_lowercase().as_str() {
        "openai" => ("https://api.openai.com", false),
        "groq" => ("https://api.groq.com/openai", false),
        "together" => ("https://api.together.xyz", false),
        "fireworks" => ("https://api.fireworks.ai/inference/v1", true),
        "mistral" => ("https://api.mistral.ai", false),
        "deepseek" => ("https://api.deepseek.com", false),
        "xai" | "grok" => ("https://api.x.ai", false),
        "openrouter" => ("https://openrouter.ai/api", false),
        "perplexity" => ("https://api.perplexity.ai", true),
        "google" | "gemini" => (
            "https://generativelanguage.googleapis.com/v1beta/openai",
            true,
        ),
        "cohere" => ("https://api.cohere.ai/compatibility/v1", true),
        _ => return None,
    };
    Some(ProviderRoute { base_url, strip_v1 })
}

/// Build the upstream URL for a proxied request.
pub fn upstream_url(route: &ProviderRoute, request_path: &str) -> String {
    let path = if route.strip_v1 {
        request_path.strip_prefix("/v1").unwrap_or(request_path)
    } else {
        request_path
    };
    format!("{}{}", route.base_url, path)
}

/// Flatten the user-supplied input text from a chat/completions-style body.
///
/// Handles OpenAI/Anthropic `messages[].content` (string or array-of-parts),
/// Anthropic top-level `system` (string or array-of-parts), and a plain
/// `prompt` string. This is the text Nova Guard scans on the input phase.
pub fn flatten_input_text(json: &Value) -> String {
    let mut out = String::new();

    match json.get("system") {
        Some(Value::String(s)) => {
            out.push_str(s);
            out.push('\n');
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }

    if let Some(prompt) = json.get("prompt").and_then(|p| p.as_str()) {
        out.push_str(prompt);
        out.push('\n');
    }

    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            match msg.get("content") {
                Some(Value::String(s)) => {
                    out.push_str(s);
                    out.push('\n');
                }
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                            out.push('\n');
                        }
                    }
                }
                _ => {}
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_openai_compatible_providers() {
        assert_eq!(
            resolve_provider("openai").unwrap(),
            ProviderRoute {
                base_url: "https://api.openai.com",
                strip_v1: false
            }
        );
        // case-insensitive
        assert_eq!(
            resolve_provider("GROQ").unwrap().base_url,
            "https://api.groq.com/openai"
        );
        assert_eq!(
            resolve_provider("grok").unwrap().base_url,
            "https://api.x.ai"
        );
        assert!(resolve_provider("gemini").unwrap().strip_v1);
        assert!(resolve_provider("perplexity").unwrap().strip_v1);
        // not pure pass-through (yet) on the edge
        assert!(resolve_provider("anthropic").is_none());
        assert!(resolve_provider("bedrock").is_none());
        assert!(resolve_provider("nope").is_none());
    }

    #[test]
    fn builds_upstream_url_with_and_without_v1_strip() {
        let openai = resolve_provider("openai").unwrap();
        assert_eq!(
            upstream_url(&openai, "/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        let gemini = resolve_provider("gemini").unwrap();
        assert_eq!(
            upstream_url(&gemini, "/v1/chat/completions"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
        let fireworks = resolve_provider("fireworks").unwrap();
        assert_eq!(
            upstream_url(&fireworks, "/v1/chat/completions"),
            "https://api.fireworks.ai/inference/v1/chat/completions"
        );
    }

    #[test]
    fn flattens_string_content_system_and_prompt() {
        let j = json!({
            "model": "gpt-4o",
            "system": "be terse",
            "messages": [{"role": "user", "content": "hello world"}]
        });
        let t = flatten_input_text(&j);
        assert!(t.contains("be terse"));
        assert!(t.contains("hello world"));

        let p = json!({"prompt": "once upon a time"});
        assert!(flatten_input_text(&p).contains("once upon a time"));
    }

    #[test]
    fn flattens_array_multimodal_and_array_system() {
        let j = json!({
            "system": [{"type": "text", "text": "sys-a"}, {"type": "text", "text": "sys-b"}],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "part-1"},
                    {"type": "image_url", "image_url": {"url": "x"}},
                    {"type": "text", "text": "part-2"}
                ]
            }]
        });
        let t = flatten_input_text(&j);
        for needle in ["sys-a", "sys-b", "part-1", "part-2"] {
            assert!(t.contains(needle), "missing {needle} in {t:?}");
        }
        assert!(!t.contains("image_url"));
    }
}
