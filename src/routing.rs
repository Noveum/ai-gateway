//! Shared request/response shaping used by BOTH the native server and the
//! Cloudflare Worker, so provider resolution, input flattening, Nova Guard text
//! transforms, and the Anthropic→OpenAI conversion behave identically on every
//! deployment shape.

use serde_json::{json, Value};

use crate::policy::{decision::Phase, PolicyEngine};

/// Base-URL override for the OpenAI upstream (the OpenAI SDK convention).
/// Declared here because both the native provider and the Worker honor it, and
/// a request must reach the same upstream whichever one serves it.
pub const OPENAI_BASE_URL_VAR: &str = "OPENAI_BASE_URL";

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
    upstream_url_with_base(route, request_path, None)
}

/// Normalize an operator-supplied base-URL override.
///
/// Trim, drop trailing slashes, and only *then* reject empty, so whitespace or a
/// bare `"///"` cannot produce a broken base URL. Byte-for-byte the rule the
/// native `OpenAIProvider::new` applies to `OPENAI_BASE_URL`, because a request
/// must resolve to the same upstream whichever deployment shape serves it.
pub fn normalize_base_url(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// [`upstream_url`], with an optional base that replaces the route's own.
///
/// The route table is a compile-time list of real provider hostnames, which
/// leaves the Worker with no way to be pointed at a local endpoint. That is not
/// only a testing inconvenience: it is why the Worker's control-plane path could
/// not be exercised end to end before shipping. `override_base` is already
/// normalized by [`normalize_base_url`]; `strip_v1` still applies, so an
/// override behaves exactly like the base it replaces.
pub fn upstream_url_with_base(
    route: &ProviderRoute,
    request_path: &str,
    override_base: Option<&str>,
) -> String {
    let path = if route.strip_v1 {
        request_path.strip_prefix("/v1").unwrap_or(request_path)
    } else {
        request_path
    };
    format!("{}{}", override_base.unwrap_or(route.base_url), path)
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

/// Apply Nova Guard input transforms (redact/mask) to every text segment in the
/// request body in place — string content, array-form (multimodal) content
/// parts, array-form `system` blocks, and a plain `prompt`. Returns true if
/// anything was mutated.
///
/// Uses the engine's transform-only path; the aggregate block decision is made
/// separately over the flattened text, so blocking is not re-litigated per
/// segment and cost/rate/token policies do not re-run here.
pub fn apply_input_transforms(engine: &PolicyEngine, model: &str, json: &mut Value) -> bool {
    let mut changed = false;
    let transform = |s: &str| engine.apply_text_transforms(Phase::Input, model, s);

    fn rewrite_string(
        v: &mut Value,
        transform: &dyn Fn(&str) -> Option<String>,
        changed: &mut bool,
    ) {
        if let Value::String(s) = v {
            if let Some(t) = transform(s) {
                if &t != s {
                    *v = Value::String(t);
                    *changed = true;
                }
            }
        }
    }

    fn rewrite_content(
        v: &mut Value,
        transform: &dyn Fn(&str) -> Option<String>,
        changed: &mut bool,
    ) {
        match v {
            Value::String(_) => rewrite_string(v, transform, changed),
            Value::Array(parts) => {
                for part in parts.iter_mut() {
                    if let Some(text) = part.get_mut("text") {
                        rewrite_string(text, transform, changed);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(prompt) = json.get_mut("prompt") {
        rewrite_string(prompt, &transform, &mut changed);
    }
    if let Some(system) = json.get_mut("system") {
        // Anthropic `system` may be a string or an array of text blocks.
        rewrite_content(system, &transform, &mut changed);
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content") {
                rewrite_content(content, &transform, &mut changed);
            }
        }
    }

    changed
}

/// Extract the assistant text from a provider response body, for the output-phase
/// scan. Handles Anthropic (`content[].text`), Google/Gemini native
/// (`candidates[].content.parts[].text`), and OpenAI-compatible
/// (`choices[].message.content`).
pub fn flatten_output_text(provider: &str, json: &Value) -> String {
    match provider {
        "anthropic" => json
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "google" | "gemini" => json
            .get("candidates")
            .and_then(|c| c.as_array())
            .map(|cands| {
                cands
                    .iter()
                    .filter_map(|c| c.get("content").and_then(|ct| ct.get("parts")))
                    .filter_map(|p| p.as_array())
                    .flat_map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        // OpenAI / compatible
        _ => json
            .get("choices")
            .and_then(|c| c.as_array())
            .map(|choices| {
                choices
                    .iter()
                    .filter_map(|c| {
                        c.get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    }
}

/// Apply Nova Guard output transforms (redact/mask) to EVERY assistant text
/// segment in a response body in place — every OpenAI `choices[].message.content`,
/// every Anthropic `content[].text`, and every Gemini
/// `candidates[].content.parts[].text`. Mirrors [`flatten_output_text`]'s full
/// traversal so multi-choice / multi-block responses can't leak unredacted text.
/// Returns true if anything was mutated.
pub fn apply_output_transforms(
    engine: &PolicyEngine,
    model: &str,
    provider: &str,
    json: &mut Value,
) -> bool {
    let mut changed = false;
    let transform = |s: &str| engine.apply_text_transforms(Phase::Output, model, s);

    fn rewrite_str(v: &mut Value, transform: &dyn Fn(&str) -> Option<String>, changed: &mut bool) {
        if let Value::String(s) = v {
            if let Some(t) = transform(s) {
                if &t != s {
                    *v = Value::String(t);
                    *changed = true;
                }
            }
        }
    }

    match provider {
        "anthropic" => {
            if let Some(parts) = json.get_mut("content").and_then(|c| c.as_array_mut()) {
                for part in parts.iter_mut() {
                    if let Some(text) = part.get_mut("text") {
                        rewrite_str(text, &transform, &mut changed);
                    }
                }
            }
        }
        "google" | "gemini" => {
            if let Some(cands) = json.get_mut("candidates").and_then(|c| c.as_array_mut()) {
                for cand in cands.iter_mut() {
                    if let Some(parts) = cand
                        .get_mut("content")
                        .and_then(|ct| ct.get_mut("parts"))
                        .and_then(|p| p.as_array_mut())
                    {
                        for part in parts.iter_mut() {
                            if let Some(text) = part.get_mut("text") {
                                rewrite_str(text, &transform, &mut changed);
                            }
                        }
                    }
                }
            }
        }
        // OpenAI / compatible
        _ => {
            if let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) {
                for choice in choices.iter_mut() {
                    if let Some(content) =
                        choice.get_mut("message").and_then(|m| m.get_mut("content"))
                    {
                        rewrite_str(content, &transform, &mut changed);
                    }
                }
            }
        }
    }

    changed
}

/// Convert an Anthropic Messages API response into the OpenAI Chat Completions
/// shape. `created_ts` is the unix-seconds timestamp for the `created` field
/// (passed in because `chrono::Utc::now()` is unavailable on wasm32).
pub fn transform_anthropic_to_openai_format(anthropic_response: Value, created_ts: i64) -> Value {
    let content =
        if let Some(content_array) = anthropic_response.get("content").and_then(|c| c.as_array()) {
            let mut text = String::new();
            for item in content_array {
                if let Some(item_text) = item.get("text").and_then(|t| t.as_str()) {
                    text.push_str(item_text);
                }
            }
            text
        } else {
            anthropic_response
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string()
        };

    let usage = {
        let mut usage_map = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});
        if let Some(anthropic_usage) = anthropic_response.get("usage") {
            if let Some(input_tokens) = anthropic_usage.get("input_tokens").and_then(|t| t.as_u64())
            {
                usage_map["prompt_tokens"] = json!(input_tokens);
            }
            if let Some(output_tokens) = anthropic_usage
                .get("output_tokens")
                .and_then(|t| t.as_u64())
            {
                usage_map["completion_tokens"] = json!(output_tokens);
            }
            let prompt_tokens = usage_map["prompt_tokens"].as_u64().unwrap_or(0);
            let completion_tokens = usage_map["completion_tokens"].as_u64().unwrap_or(0);
            usage_map["total_tokens"] = json!(prompt_tokens + completion_tokens);
        }
        usage_map
    };

    let finish_reason = match anthropic_response
        .get("stop_reason")
        .and_then(|r| r.as_str())
    {
        Some("end_turn") => "stop",
        Some("max_tokens") => "length",
        Some("stop_sequence") => "stop",
        Some(reason) => reason,
        None => "stop",
    };

    let mut transformed = json!({
        "id": anthropic_response.get("id").unwrap_or(&Value::Null),
        "object": "chat.completion",
        "created": created_ts,
        "model": anthropic_response.get("model").unwrap_or(&Value::Null),
        "type": anthropic_response.get("type").unwrap_or(&json!("message")),
        "role": anthropic_response.get("role").unwrap_or(&json!("assistant")),
        "choices": [{
            "index": 0,
            "message": {
                "role": anthropic_response.get("role").unwrap_or(&json!("assistant")),
                "content": content
            },
            "finish_reason": finish_reason
        }],
        "usage": usage,
        "system_fingerprint": format!("anthropic-{}", anthropic_response.get("model").and_then(|m| m.as_str()).unwrap_or("claude"))
    });

    if let Some(seed) = anthropic_response.get("seed") {
        if let Some(choices) = transformed
            .get_mut("choices")
            .and_then(|c| c.as_array_mut())
        {
            for choice in choices {
                if seed.is_number() {
                    choice["seed"] = json!(seed.to_string());
                } else {
                    choice["seed"] = seed.clone();
                }
            }
        }
    }

    transformed
}

/// Convert an OpenAI chat-completions body into the AWS Bedrock **Converse** API
/// request shape. Mirrors the native `BedrockProvider::transform_request_body`
/// (defaults: maxTokens 1000, temperature 0.7, topP 1.0).
pub fn openai_to_bedrock_converse(body: &Value) -> Value {
    // Already Converse-shaped → pass through.
    if body.get("inferenceConfig").is_some() {
        return body.clone();
    }

    // Extract a message's text from string OR array-of-parts content.
    fn content_text(msg: &Value) -> String {
        match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }

    // Converse keeps `system` separate and only allows user/assistant roles in
    // `messages`; route `system` entries to the top-level `system` field so they
    // are not dropped or rejected.
    let mut messages = Vec::new();
    let mut system = Vec::new();
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = content_text(msg);
            if role == "system" {
                system.push(json!({ "text": text }));
            } else {
                messages.push(json!({ "role": role, "content": [{ "text": text }] }));
            }
        }
    }

    let mut inference = json!({
        "maxTokens": body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(1000),
        "temperature": body.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7),
        "topP": body.get("top_p").and_then(|v| v.as_f64()).unwrap_or(1.0),
    });
    // OpenAI `stop` (string or array) → Converse `inferenceConfig.stopSequences`.
    match body.get("stop") {
        Some(Value::String(s)) => inference["stopSequences"] = json!([s]),
        Some(Value::Array(a)) => {
            let seqs: Vec<&str> = a.iter().filter_map(|v| v.as_str()).collect();
            if !seqs.is_empty() {
                inference["stopSequences"] = json!(seqs);
            }
        }
        _ => {}
    }

    let mut out = json!({ "messages": messages, "inferenceConfig": inference });
    if !system.is_empty() {
        out["system"] = Value::Array(system);
    }
    out
}

/// Convert an AWS Bedrock **Converse** API response into OpenAI Chat Completions
/// shape. Mirrors the native `BedrockProvider::transform_bedrock_to_openai_format`.
/// `created_ts` is passed in (chrono is unavailable on wasm32).
pub fn bedrock_converse_to_openai(resp: &Value, model: &str, created_ts: i64) -> Value {
    // Concatenate ALL text blocks (Converse may return several), not just the
    // first, so multi-block responses are not silently truncated.
    let content = resp
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    let usage = resp.get("usage");
    let tok = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };

    let finish_reason = match resp.get("stopReason").and_then(|s| s.as_str()) {
        Some("end_turn") => "stop",
        Some("max_tokens") | Some("model_context_window_exceeded") => "length",
        Some("stop_sequence") => "stop",
        // Surface safety stops distinctly instead of as a normal "stop".
        Some("content_filtered") | Some("guardrail_intervened") => "content_filter",
        _ => "stop",
    };

    json!({
        "id": format!("chatcmpl-bedrock-{created_ts}"),
        "object": "chat.completion",
        "created": created_ts,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": tok("inputTokens"),
            "completion_tokens": tok("outputTokens"),
            "total_tokens": tok("totalTokens"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The override has to normalize identically on both deployment shapes, or
    /// the same `OPENAI_BASE_URL` sends a request to two different upstreams.
    #[test]
    fn a_base_url_override_is_normalized_then_rejected_if_empty() {
        assert_eq!(
            normalize_base_url(Some("  http://127.0.0.1:8899///  ")).as_deref(),
            Some("http://127.0.0.1:8899")
        );
        assert_eq!(
            normalize_base_url(Some("https://api.test.dev")).as_deref(),
            Some("https://api.test.dev")
        );
        // Normalize FIRST, reject empty second: whitespace and a bare "///"
        // must not survive as a base URL.
        for empty in [None, Some(""), Some("   "), Some("///"), Some("  //  ")] {
            assert_eq!(normalize_base_url(empty), None, "{empty:?} must not apply");
        }
    }

    #[test]
    fn an_override_replaces_the_base_and_still_honors_strip_v1() {
        let openai = resolve_provider("openai").unwrap();
        assert_eq!(
            upstream_url(&openai, "/v1/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            upstream_url_with_base(
                &openai,
                "/v1/chat/completions",
                Some("http://127.0.0.1:8899")
            ),
            "http://127.0.0.1:8899/v1/chat/completions"
        );
        // No override is exactly the old behaviour.
        assert_eq!(
            upstream_url_with_base(&openai, "/v1/chat/completions", None),
            upstream_url(&openai, "/v1/chat/completions")
        );
        // `strip_v1` belongs to the route, not the base, so an override on a
        // stripping provider still drops the version segment.
        let gemini = resolve_provider("gemini").unwrap();
        assert!(gemini.strip_v1);
        assert_eq!(
            upstream_url_with_base(
                &gemini,
                "/v1/chat/completions",
                Some("http://127.0.0.1:8899")
            ),
            "http://127.0.0.1:8899/chat/completions"
        );
    }

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

    #[test]
    fn flattens_output_for_openai_anthropic_and_gemini() {
        let openai = json!({"choices": [{"message": {"content": "oai-out"}}]});
        assert_eq!(flatten_output_text("openai", &openai), "oai-out");

        let anthropic = json!({"content": [{"type": "text", "text": "ant-a"}, {"type": "text", "text": "ant-b"}]});
        assert_eq!(flatten_output_text("anthropic", &anthropic), "ant-a\nant-b");

        let gemini = json!({"candidates": [{"content": {"parts": [{"text": "gem-out"}]}}]});
        assert_eq!(flatten_output_text("gemini", &gemini), "gem-out");
    }

    #[test]
    fn apply_output_transforms_redacts_every_segment() {
        use crate::policy::config::PolicyBundle;
        use crate::policy::engine::EngineOptions;

        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"redact-out","type":"regex_match","mode":"enforce","config":{"phase":"output","patterns":[{"name":"s","regex":"secret"}],"action":"redact","redactWith":"[X]"}}]}"#,
        )
        .expect("bundle parses");
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());

        // OpenAI: BOTH choices must be redacted (regression: only the first was).
        let mut openai = json!({"choices": [
            {"message": {"role": "assistant", "content": "secret one"}},
            {"message": {"role": "assistant", "content": "secret two"}}
        ]});
        assert!(apply_output_transforms(
            &engine,
            "gpt-4o",
            "openai",
            &mut openai
        ));
        assert_eq!(openai["choices"][0]["message"]["content"], "[X] one");
        assert_eq!(openai["choices"][1]["message"]["content"], "[X] two");

        // Anthropic: every text block.
        let mut anthropic = json!({"content": [
            {"type": "text", "text": "secret a"},
            {"type": "text", "text": "secret b"}
        ]});
        assert!(apply_output_transforms(
            &engine,
            "claude",
            "anthropic",
            &mut anthropic
        ));
        assert_eq!(anthropic["content"][0]["text"], "[X] a");
        assert_eq!(anthropic["content"][1]["text"], "[X] b");

        // No match → false.
        let mut clean = json!({"choices": [{"message": {"content": "all clear"}}]});
        assert!(!apply_output_transforms(
            &engine, "gpt-4o", "openai", &mut clean
        ));
    }

    #[test]
    fn anthropic_to_openai_maps_content_usage_finish_and_created() {
        let anthropic = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "Hello "}, {"type": "text", "text": "world"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        let out = transform_anthropic_to_openai_format(anthropic, 1_700_000_000);
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["created"], 1_700_000_000);
        assert_eq!(out["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(out["choices"][0]["finish_reason"], "length");
        assert_eq!(out["usage"]["total_tokens"], 7);
    }

    #[test]
    fn apply_input_transforms_redacts_via_engine() {
        use crate::policy::config::PolicyBundle;
        use crate::policy::engine::EngineOptions;

        // A redact policy that masks the literal "secret" in the input phase,
        // using a custom `redactWith` replacement.
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"redact-secret","type":"regex_match","mode":"enforce","config":{"phase":"input","patterns":[{"name":"sec","regex":"secret"}],"action":"redact","redactWith":"[MASKED]"}}]}"#,
        )
        .expect("bundle parses");
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());

        let mut body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "my secret is here"}]
        });
        let changed = apply_input_transforms(&engine, "gpt-4o", &mut body);
        assert!(changed, "expected redaction to mutate the body");
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("[MASKED]"), "got: {content}");
        assert!(!content.contains("secret"));
    }

    #[test]
    fn openai_to_bedrock_converse_shapes_messages_and_config() {
        let body = json!({
            "model": "amazon.titan-text-premier-v1:0",
            "max_tokens": 256,
            "temperature": 0.2,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let out = openai_to_bedrock_converse(&body);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(out["inferenceConfig"]["maxTokens"], 256);
        assert_eq!(out["inferenceConfig"]["temperature"], 0.2);
        assert_eq!(out["inferenceConfig"]["topP"], 1.0); // default
                                                         // Already-Converse bodies pass through unchanged.
        let passthrough = json!({"messages": [], "inferenceConfig": {"maxTokens": 1}});
        assert_eq!(openai_to_bedrock_converse(&passthrough), passthrough);
    }

    #[test]
    fn bedrock_converse_to_openai_maps_content_usage_and_finish() {
        let resp = json!({
            "output": {"message": {"content": [{"text": "hi there"}]}},
            "stopReason": "max_tokens",
            "usage": {"inputTokens": 7, "outputTokens": 3, "totalTokens": 10}
        });
        let out =
            bedrock_converse_to_openai(&resp, "amazon.titan-text-premier-v1:0", 1_700_000_000);
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["created"], 1_700_000_000);
        assert_eq!(out["choices"][0]["message"]["content"], "hi there");
        assert_eq!(out["choices"][0]["finish_reason"], "length");
        assert_eq!(out["usage"]["prompt_tokens"], 7);
        assert_eq!(out["usage"]["completion_tokens"], 3);
        assert_eq!(out["usage"]["total_tokens"], 10);
    }

    #[test]
    fn openai_to_bedrock_converse_handles_system_array_content_and_stop() {
        let body = json!({
            "model": "amazon.nova-micro-v1:0",
            "stop": ["END", "STOP"],
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": [
                    {"type": "text", "text": "part-1"},
                    {"type": "image_url", "image_url": {"url": "x"}},
                    {"type": "text", "text": " part-2"}
                ]}
            ]
        });
        let out = openai_to_bedrock_converse(&body);
        // system routed to the top-level field, NOT into messages.
        assert_eq!(out["system"][0]["text"], "be terse");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
        // array (multimodal) content text is preserved, not dropped to empty.
        assert_eq!(out["messages"][0]["content"][0]["text"], "part-1 part-2");
        // stop → stopSequences.
        assert_eq!(
            out["inferenceConfig"]["stopSequences"],
            json!(["END", "STOP"])
        );
    }

    #[test]
    fn bedrock_converse_aggregates_blocks_and_maps_safety_stops() {
        let resp = json!({
            "output": {"message": {"content": [{"text": "a"}, {"text": "b"}, {"text": "c"}]}},
            "stopReason": "guardrail_intervened",
            "usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}
        });
        let out = bedrock_converse_to_openai(&resp, "m", 1);
        // ALL content blocks concatenated (regression: only the first was kept).
        assert_eq!(out["choices"][0]["message"]["content"], "abc");
        // safety stop surfaced distinctly.
        assert_eq!(out["choices"][0]["finish_reason"], "content_filter");
    }
}
