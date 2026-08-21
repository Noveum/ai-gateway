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
/// Base-URL override for Anthropic's native Messages API. Used by both runtimes
/// for hermetic transport tests and private compatible endpoints.
pub const ANTHROPIC_BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

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

/// Ensure a strict OpenAI cost cap is not applied to an endpoint whose billing
/// contract differs from the standard catalog. OpenAI's US/EU regional
/// processing hosts add a 10% uplift for eligible models, and an arbitrary
/// compatible endpoint can have any price card. Loopback is retained solely so
/// hermetic native/workerd transport tests can exercise the real strict path.
pub fn validate_strict_openai_base_url(raw: Option<&str>) -> Result<(), String> {
    let Some(base) = normalize_base_url(raw).map(|base| base.to_ascii_lowercase()) else {
        return Ok(());
    };
    if base == "https://api.openai.com" || strict_base_url_is_loopback(&base) {
        Ok(())
    } else {
        Err(
            "the configured OpenAI base URL is unsupported under a strict cost cap because regional or compatible-endpoint pricing is not the standard OpenAI catalog rate"
                .to_string(),
        )
    }
}

/// Anthropic overrides have the same strict-contract boundary: only Anthropic's
/// canonical billed endpoint (plus loopback transport fixtures) uses the rates
/// compiled into this gateway.
pub fn validate_strict_anthropic_base_url(raw: Option<&str>) -> Result<(), String> {
    let Some(base) = normalize_base_url(raw).map(|base| base.to_ascii_lowercase()) else {
        return Ok(());
    };
    if base == "https://api.anthropic.com" || strict_base_url_is_loopback(&base) {
        Ok(())
    } else {
        Err(
            "the configured Anthropic base URL is unsupported under a strict cost cap because compatible-endpoint pricing is not the Anthropic catalog rate"
                .to_string(),
        )
    }
}

fn strict_base_url_is_loopback(base: &str) -> bool {
    [
        "http://127.0.0.1:",
        "https://127.0.0.1:",
        "http://localhost:",
        "https://localhost:",
        "http://[::1]:",
        "https://[::1]:",
    ]
    .iter()
    .any(|prefix| base.starts_with(prefix))
}

/// Extract one RFC-style Bearer credential without accepting another auth
/// scheme or smuggling whitespace-delimited data into the provider API key.
/// Shared by native and Worker Anthropic adapters so their auth contracts stay
/// identical.
pub fn authorization_bearer_token(value: &str) -> Option<&str> {
    let mut parts = value.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(token)
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

/// Extra strict-mode input reservation when a request enables client tools.
///
/// Providers inject a tool-use system prompt that is not present in caller
/// JSON. Anthropic's largest currently published value is 804 tokens; 4,096
/// leaves headroom for provider/version drift while the response settlement
/// still reconciles the hold to exact reported usage.
pub const STRICT_TOOL_PROMPT_RESERVE_TOKENS: u32 = 4_096;

/// Estimate prompt tokens for admission in native and Worker runtimes.
///
/// Advisory mode preserves the historical chars/4 heuristic. Strict mode
/// instead treats every serialized byte of the *post-transform forward body*
/// as one token. Byte-fallback tokenizers cannot create more text/schema tokens
/// than input bytes, and this includes fields the old text flattener omitted:
/// tool schemas, tool arguments, Responses `input`, and inline media. A
/// separate allowance covers the provider-injected tool system prompt.
pub fn estimate_admission_input_tokens(body: &Value, strict: bool) -> u32 {
    if !strict {
        return flatten_input_text(body)
            .chars()
            .count()
            .div_ceil(4)
            .min(u32::MAX as usize) as u32;
    }

    let serialized_bytes = serde_json::to_vec(body)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        .min(u32::MAX as usize) as u32;
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
        || body
            .get("functions")
            .and_then(Value::as_array)
            .is_some_and(|functions| !functions.is_empty());
    serialized_bytes.saturating_add(if has_tools {
        STRICT_TOOL_PROMPT_RESERVE_TOKENS
    } else {
        0
    })
}

/// Resolve every accepted output-limit spelling before strict admission.
/// Multiple aliases are harmless only when they declare the same ceiling;
/// otherwise the admission hold and the provider could select different
/// values from the same request.
fn strict_output_limit_alias(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<u64>, String> {
    let mut resolved: Option<(&str, u64)> = None;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let limit = value
            .as_u64()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| format!("{key} must be a positive integer"))?;
        if let Some((resolved_key, resolved_limit)) = resolved {
            if limit != resolved_limit {
                return Err(format!(
                    "output limit aliases `{resolved_key}` and `{key}` must agree under a strict cost cap"
                ));
            }
        } else {
            resolved = Some((key, limit));
        }
    }
    Ok(resolved.map(|(_, limit)| limit))
}

/// Validate and prepare the supported strict-admission request surface.
///
/// Strict cost caps intentionally support only bounded JSON Chat Completions.
/// Broader pass-through remains available under advisory policies, but stateful
/// Responses, provider-fetched media/search, and providers with mandatory
/// request fees cannot be represented by the current single-model token hold.
/// For direct OpenAI calls we pin the standard service tier so an account-level
/// priority default cannot silently double a request after it was reserved.
pub fn prepare_strict_admission_body(
    provider: &str,
    request_path: &str,
    body: &mut Value,
) -> Result<u32, String> {
    if request_path != "/v1/chat/completions" {
        return Err(
            "strict cost caps currently support only JSON /v1/chat/completions requests"
                .to_string(),
        );
    }
    if provider.eq_ignore_ascii_case("perplexity") {
        return Err(
            "Perplexity is unsupported under a strict cost cap until its per-request and search fees are reserved and settled"
                .to_string(),
        );
    }
    if provider.eq_ignore_ascii_case("openrouter") {
        return Err(
            "OpenRouter is unsupported under a strict cost cap until routed-model and plugin fees are reserved and settled"
                .to_string(),
        );
    }

    let object = body
        .as_object_mut()
        .ok_or_else(|| "strict cost caps require a JSON object request body".to_string())?;
    if matches!(provider.to_ascii_lowercase().as_str(), "xai" | "grok")
        && object.contains_key("search_parameters")
    {
        return Err(
            "xAI search_parameters is unsupported under a strict cost cap because server-side search fees and source usage are not represented by the token hold"
                .to_string(),
        );
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| "model must be a non-empty string".to_string())?
        .to_ascii_lowercase();
    if provider.eq_ignore_ascii_case("bedrock")
        && !crate::policy::pricing::bedrock_model_has_strict_pricing(&model)
    {
        return Err(
            "this Bedrock model is unsupported under a strict cost cap until its source-region-aware pricing is reserved and settled"
                .to_string(),
        );
    }
    if provider.eq_ignore_ascii_case("groq")
        && matches!(model.as_str(), "groq/compound" | "groq/compound-mini")
    {
        return Err(
            "Groq Compound systems are unsupported under a strict cost cap because their built-in search and code-execution work is not represented by the token hold"
                .to_string(),
        );
    }
    fn contains_nested_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Array(values) => values.iter().any(|value| contains_nested_key(value, key)),
            Value::Object(object) => {
                object.contains_key(key)
                    || object.values().any(|value| contains_nested_key(value, key))
            }
            _ => false,
        }
    }
    if !object.get("messages").is_some_and(Value::is_array) {
        return Err("messages must be an array".to_string());
    }
    let strict_output_limit = strict_output_limit_alias(object)?;
    if object
        .get("n")
        .is_some_and(|value| value.as_u64() != Some(1))
    {
        return Err("n must be 1 under a strict cost cap".to_string());
    }
    if object
        .get("best_of")
        .is_some_and(|value| value.as_u64() != Some(1))
    {
        return Err("best_of must be 1 under a strict cost cap".to_string());
    }
    if object.contains_key("prompt_cache_retention") {
        return Err(
            "prompt_cache_retention is unsupported under a strict cost cap until its cache-write premium is modeled"
                .to_string(),
        );
    }
    if object.contains_key("previous_response_id") || object.contains_key("conversation") {
        return Err(
            "server-side conversation state is unsupported under a strict cost cap because prior context is not bounded by this request"
                .to_string(),
        );
    }
    if provider.eq_ignore_ascii_case("bedrock")
        && [
            "inferenceConfig",
            "additionalModelRequestFields",
            "guardrailConfig",
            "performanceConfig",
        ]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Err(
            "provider-native Bedrock request fields are unsupported under a strict cost cap; use the bounded OpenAI chat-completions fields instead"
                .to_string(),
        );
    }
    if provider.eq_ignore_ascii_case("bedrock")
        && (object
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
            || object
                .get("functions")
                .and_then(Value::as_array)
                .is_some_and(|functions| !functions.is_empty())
            || object
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message.get("role").and_then(Value::as_str) == Some("tool")
                            || message.get("tool_calls").is_some()
                            || message.get("function_call").is_some()
                    })
                }))
    {
        return Err(
            "Bedrock tool use is unsupported under a strict cost cap because the current Converse adapter does not translate tool definitions or tool-call history"
                .to_string(),
        );
    }
    if matches!(provider.to_ascii_lowercase().as_str(), "google" | "gemini")
        && object
            .get("extra_body")
            .and_then(|value| value.get("google"))
            .and_then(|value| value.get("cached_content"))
            .is_some()
    {
        return Err(
            "Gemini cached context is unsupported under a strict cost cap because provider-side cached content is not bounded by this request"
                .to_string(),
        );
    }

    if provider.eq_ignore_ascii_case("openai") {
        if object
            .values()
            .any(|value| contains_nested_key(value, "prompt_cache_breakpoint"))
        {
            return Err(
                "prompt_cache_breakpoint is unsupported under a strict cost cap until explicit cache-write premiums are reserved"
                    .to_string(),
            );
        }
        let supports_implicit_cache_writes = model == "gpt-5.6"
            || model.starts_with("gpt-5.6-")
            || matches!(
                model.as_str(),
                "daybreak-blue-latest" | "daybreak-red-latest"
            );
        match object.get("prompt_cache_options") {
            None if supports_implicit_cache_writes => {
                // GPT-5.6+ otherwise creates an implicit cache breakpoint whose
                // first write costs 1.25x input. Force explicit mode with no
                // breakpoints so the catalog's ordinary-input hold is exact.
                object.insert(
                    "prompt_cache_options".to_string(),
                    json!({"mode": "explicit"}),
                );
            }
            None => {}
            Some(options)
                if supports_implicit_cache_writes
                    && options.get("mode").and_then(Value::as_str) == Some("explicit") => {}
            Some(_) => {
                return Err(
                    "prompt cache writes are unsupported under a strict cost cap; use prompt_cache_options.mode=explicit without breakpoints"
                        .to_string(),
                );
            }
        }
        // `max_output_tokens` is the gateway's cross-provider alias, not a
        // Chat Completions field. Preserve either native OpenAI limit when the
        // caller supplied one; otherwise map the alias to the current field so
        // the bounded request admitted here is also accepted upstream.
        for key in ["max_tokens", "max_completion_tokens"] {
            if object.get(key).is_some_and(Value::is_null) {
                object.remove(key);
            }
        }
        if let Some(alias) = object.remove("max_output_tokens") {
            if !object.contains_key("max_tokens") && !object.contains_key("max_completion_tokens") {
                object.insert("max_completion_tokens".to_string(), alias);
            }
        }
        match object.get("service_tier").and_then(Value::as_str) {
            None | Some("default") => {
                object.insert("service_tier".to_string(), Value::String("default".to_string()));
            }
            Some(_) => {
                return Err(
                    "service_tier must be default under a strict cost cap because premium tiers are not in the standard model rate"
                        .to_string(),
                )
            }
        }
    } else if object
        .get("service_tier")
        .and_then(Value::as_str)
        .is_some_and(|tier| tier != "default")
    {
        return Err("non-default service_tier is unsupported under a strict cost cap".to_string());
    }

    // These providers receive the request unchanged on the OpenAI-compatible
    // Chat Completions wire. `max_output_tokens` is a gateway alias, so collapse
    // every equivalent spelling into the widely supported `max_tokens` field.
    // The value is exactly the ceiling resolved by admission above.
    if matches!(
        provider.to_ascii_lowercase().as_str(),
        "groq"
            | "together"
            | "fireworks"
            | "mistral"
            | "deepseek"
            | "xai"
            | "grok"
            | "google"
            | "gemini"
            | "cohere"
    ) {
        for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            object.remove(key);
        }
        if let Some(limit) = strict_output_limit {
            object.insert("max_tokens".to_string(), json!(limit));
        }
    }

    validate_strict_admission_input(body)?;
    Ok(estimate_admission_input_tokens(body, true))
}

/// Reject strict-mode inputs whose provider-side expansion cannot be bounded
/// from the request bytes. Advisory mode can still proxy these shapes and
/// reconcile reported usage; an atomic strict hold cannot honestly call them a
/// maximum before an external image/file/search/audio workload has run.
pub fn validate_strict_admission_input(body: &Value) -> Result<(), String> {
    if body.get("mcp_servers").is_some() {
        return Err(
            "mcp_servers is unsupported under a strict cost cap because remote MCP context and tool work are not bounded by the request"
                .to_string(),
        );
    }
    if body.get("web_search_options").is_some() {
        return Err(
            "web_search_options is unsupported under a strict cost cap because provider-fetched search context is not bounded by the request"
                .to_string(),
        );
    }
    if body.get("attachments").is_some() || body.get("files").is_some() {
        return Err(
            "file input is unsupported under a strict cost cap because referenced content is not bounded by the request"
                .to_string(),
        );
    }
    if body.get("audio").is_some()
        || body
            .get("modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| modalities.iter().any(|v| v.as_str() == Some("audio")))
    {
        return Err(
            "audio input or output is unsupported under a strict cost cap because audio pricing is not represented by the text-token reservation"
                .to_string(),
        );
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let kind = tool.get("type").and_then(Value::as_str);
            match kind {
                Some("function") => {}
                Some(kind) => {
                    return Err(format!(
                        "server-side tool `{kind}` is unsupported under a strict cost cap because its provider-side work is not bounded by the request"
                    ));
                }
                None => {
                    return Err(
                        "a tool without OpenAI type=function is unsupported under a strict cost cap because provider-native tool work is not bounded by the request"
                            .to_string(),
                    );
                }
            }
        }
    }

    fn visit_content(value: &Value) -> Result<(), String> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit_content(value)?;
                }
            }
            Value::Object(object) => {
                let kind = object.get("type").and_then(Value::as_str);
                if matches!(
                    kind,
                    Some("input_file" | "file" | "document" | "document_url")
                ) || ["file_id", "file_url", "document_url"]
                    .iter()
                    .any(|field| object.get(*field).is_some())
                {
                    return Err(
                        "file or document input is unsupported under a strict cost cap because referenced content is not bounded by the request"
                            .to_string(),
                    );
                }
                if matches!(kind, Some("input_audio" | "audio" | "audio_url"))
                    || object.get("audio_url").is_some()
                {
                    return Err(
                        "audio input is unsupported under a strict cost cap because audio pricing is not represented by the text-token reservation"
                            .to_string(),
                    );
                }
                if matches!(kind, Some("input_video" | "video" | "video_url"))
                    || object.get("video_url").is_some()
                {
                    return Err(
                        "video input is unsupported under a strict cost cap because fetched media and video pricing are not represented by the text-token reservation"
                            .to_string(),
                    );
                }
                if matches!(kind, Some("image_url" | "input_image")) {
                    let image_url = object.get("image_url").and_then(|value| match value {
                        Value::String(url) => Some(url.as_str()),
                        Value::Object(image) => image.get("url").and_then(Value::as_str),
                        _ => None,
                    });
                    if image_url.is_some_and(|url| {
                        url.starts_with("http://") || url.starts_with("https://")
                    }) {
                        return Err(
                            "remote image input is unsupported under a strict cost cap because fetched pixels are not bounded by the request"
                                .to_string(),
                        );
                    }
                    return Err(
                        "image input is unsupported under a strict cost cap because visual-token pricing is not represented by the text-token reservation"
                            .to_string(),
                    );
                }
                if object
                    .get("source")
                    .and_then(Value::as_object)
                    .is_some_and(|source| source.get("type").and_then(Value::as_str) == Some("url"))
                {
                    return Err(
                        "remote image input is unsupported under a strict cost cap because fetched pixels are not bounded by the request"
                            .to_string(),
                    );
                }
                for nested in object.values() {
                    visit_content(nested)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    if let Some(input) = body.get("input") {
        visit_content(input)?;
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if message.get("attachments").is_some() {
                return Err(
                    "file input is unsupported under a strict cost cap because referenced content is not bounded by the request"
                        .to_string(),
                );
            }
            if let Some(content) = message.get("content") {
                visit_content(content)?;
            }
        }
    }
    Ok(())
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
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(content_array) = anthropic_response.get("content").and_then(Value::as_array) {
        for item in content_array {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(item_text) = item.get("text").and_then(Value::as_str) {
                        text.push_str(item_text);
                    }
                }
                Some("tool_use") => {
                    let Some(id) = item
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    let Some(name) = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    else {
                        continue;
                    };
                    let input = item.get("input").cloned().unwrap_or_else(|| json!({}));
                    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }));
                }
                _ => {}
            }
        }
    } else if let Some(content) = anthropic_response.get("content").and_then(Value::as_str) {
        text.push_str(content);
    }

    let usage =
        if let Some(anthropic_usage) = anthropic_response.get("usage").and_then(Value::as_object) {
            let mut usage_map = serde_json::Map::new();
            let input_tokens = anthropic_usage.get("input_tokens").and_then(Value::as_u64);
            let output_tokens = anthropic_usage.get("output_tokens").and_then(Value::as_u64);
            if let Some(tokens) = input_tokens {
                usage_map.insert("prompt_tokens".to_string(), json!(tokens));
            }
            if let Some(tokens) = output_tokens {
                usage_map.insert("completion_tokens".to_string(), json!(tokens));
            }
            if let (Some(input), Some(output)) = (input_tokens, output_tokens) {
                usage_map.insert(
                    "total_tokens".to_string(),
                    json!(input.saturating_add(output)),
                );
            }

            // Keep Anthropic's disjoint cache and server-tool dimensions on the
            // translated body. OpenAI clients ignore unknown usage keys, while
            // NovaGuard's provider-aware pricer needs them for exact settlement.
            for field in [
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "cache_creation",
                "server_tool_use",
                "inference_geo",
                "speed",
            ] {
                if let Some(value) = anthropic_usage.get(field) {
                    usage_map.insert(field.to_string(), value.clone());
                }
            }
            let cache_read = anthropic_usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64);
            let cache_write = anthropic_usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64);
            if cache_read.is_some() || cache_write.is_some() {
                let mut details = serde_json::Map::new();
                if let Some(tokens) = cache_read {
                    details.insert("cached_tokens".to_string(), json!(tokens));
                }
                if let Some(tokens) = cache_write {
                    details.insert("cache_write_tokens".to_string(), json!(tokens));
                }
                usage_map.insert("prompt_tokens_details".to_string(), Value::Object(details));
            }
            if anthropic_response
                .get("stop_reason")
                .and_then(Value::as_str)
                == Some("refusal")
                && output_tokens == Some(0)
            {
                usage_map.insert("unbilled_refusal".to_string(), Value::Bool(true));
            }
            Value::Object(usage_map)
        } else {
            Value::Null
        };

    let finish_reason = match anthropic_response
        .get("stop_reason")
        .and_then(|r| r.as_str())
    {
        Some("end_turn") => "stop",
        Some("max_tokens" | "model_context_window_exceeded") => "length",
        Some("stop_sequence") => "stop",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        Some("pause_turn") => "stop",
        Some(_) => "stop",
        None => "stop",
    };

    let role = anthropic_response
        .get("role")
        .cloned()
        .unwrap_or_else(|| json!("assistant"));
    let mut message = json!({
        "role": role,
        "content": if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let mut transformed = json!({
        "id": anthropic_response.get("id").unwrap_or(&Value::Null),
        "object": "chat.completion",
        "created": created_ts,
        "model": anthropic_response.get("model").unwrap_or(&Value::Null),
        "type": anthropic_response.get("type").unwrap_or(&json!("message")),
        "role": anthropic_response.get("role").unwrap_or(&json!("assistant")),
        "choices": [{
            "index": 0,
            "message": message,
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

fn openai_image_part_to_anthropic(
    part: &Value,
    message_index: usize,
    part_index: usize,
) -> Result<Value, String> {
    let prefix = format!("messages[{message_index}].content[{part_index}].image_url");
    let image_url = part
        .get("image_url")
        .ok_or_else(|| format!("{prefix} is required"))?;
    let url = match image_url {
        Value::String(url) => Some(url.as_str()),
        Value::Object(image) => image.get("url").and_then(Value::as_str),
        _ => None,
    }
    .filter(|url| !url.is_empty())
    .ok_or_else(|| format!("{prefix}.url must be a non-empty string"))?;

    if let Some(data_url) = url.strip_prefix("data:") {
        let (media_type, data) = data_url
            .split_once(";base64,")
            .ok_or_else(|| format!("{prefix}.url must contain a base64 data URL"))?;
        if !matches!(
            media_type,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ) {
            return Err(format!("{prefix}.url uses an unsupported image media type"));
        }
        if data.is_empty() {
            return Err(format!("{prefix}.url contains empty base64 data"));
        }
        return Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }));
    }

    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!(
            "{prefix}.url must be an HTTP(S) URL or a supported base64 data URL"
        ));
    }
    Ok(json!({
        "type": "image",
        "source": {"type": "url", "url": url},
    }))
}

fn normalize_anthropic_message_content(
    content: Value,
    role: &str,
    message_index: usize,
    permits_null: bool,
) -> Result<Value, String> {
    match content {
        Value::String(_) => Ok(content),
        Value::Null if permits_null => Ok(Value::Null),
        Value::Array(parts) => {
            let mut normalized = Vec::with_capacity(parts.len());
            for (part_index, part) in parts.into_iter().enumerate() {
                let part_type = part.get("type").and_then(Value::as_str).ok_or_else(|| {
                    format!("messages[{message_index}].content[{part_index}].type must be a string")
                })?;
                match part_type {
                    "text" => {
                        if part.get("text").and_then(Value::as_str).is_none() {
                            return Err(format!(
                                "messages[{message_index}].content[{part_index}].text must be a string"
                            ));
                        }
                        normalized.push(part);
                    }
                    "image_url" if role == "user" => normalized.push(
                        openai_image_part_to_anthropic(&part, message_index, part_index)?,
                    ),
                    "image" if role == "user" => {
                        if !part.get("source").is_some_and(Value::is_object) {
                            return Err(format!(
                                "messages[{message_index}].content[{part_index}].source must be an object"
                            ));
                        }
                        normalized.push(part);
                    }
                    "tool_use" if role == "assistant" => normalized.push(part),
                    "tool_result" if role == "user" => normalized.push(part),
                    "thinking" | "redacted_thinking" if role == "assistant" => {
                        normalized.push(part)
                    }
                    _ => {
                        return Err(format!(
                            "messages[{message_index}].content[{part_index}].type is unsupported for role {role}: {part_type}"
                        ));
                    }
                }
            }
            Ok(Value::Array(normalized))
        }
        _ => Err(format!(
            "messages[{message_index}].content must be a string or an array"
        )),
    }
}

fn anthropic_model_is(model: &str, family: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model == family
        || model.strip_prefix(family).is_some_and(|suffix| {
            suffix
                .as_bytes()
                .first()
                .is_some_and(|separator| matches!(separator, b'-' | b':' | b'.' | b'@' | b'/'))
        })
}

/// Anthropic's current Messages contract rejects non-default sampling on these
/// model families. Keep this list tied to the provider's own compatibility
/// table rather than treating every Claude model as equivalent:
/// <https://platform.claude.com/docs/en/about-claude/models/extended-thinking-models#limits-and-feature-compatibility>
fn anthropic_has_fixed_sampling(model: &str) -> bool {
    [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
    ]
    .iter()
    .any(|family| anthropic_model_is(model, family))
}

fn validate_anthropic_cache_control(value: &Value, path: &str) -> Result<(), String> {
    let cache = value
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    if cache.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return Err(format!("{path}.type must be ephemeral"));
    }
    if let Some(ttl) = cache.get("ttl") {
        if !ttl.as_str().is_some_and(|ttl| matches!(ttl, "5m" | "1h")) {
            return Err(format!("{path}.ttl must be 5m or 1h"));
        }
    }
    Ok(())
}

fn validate_anthropic_cache_controls(
    object: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    if let Some(cache) = object.get("cache_control") {
        validate_anthropic_cache_control(cache, "cache_control")?;
    }
    if let Some(system) = object.get("system").and_then(Value::as_array) {
        for (index, block) in system.iter().enumerate() {
            if let Some(cache) = block.get("cache_control") {
                validate_anthropic_cache_control(cache, &format!("system[{index}].cache_control"))?;
            }
        }
    }
    if let Some(messages) = object.get("messages").and_then(Value::as_array) {
        for (message_index, message) in messages.iter().enumerate() {
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                for (part_index, part) in parts.iter().enumerate() {
                    if let Some(cache) = part.get("cache_control") {
                        validate_anthropic_cache_control(
                            cache,
                            &format!(
                                "messages[{message_index}].content[{part_index}].cache_control"
                            ),
                        )?;
                    }
                }
            }
        }
    }
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        for (index, tool) in tools.iter().enumerate() {
            if let Some(cache) = tool.get("cache_control") {
                validate_anthropic_cache_control(cache, &format!("tools[{index}].cache_control"))?;
            }
        }
    }
    Ok(())
}

/// Convert an OpenAI Chat Completions request into Anthropic's Messages shape.
pub fn openai_to_anthropic_messages(body: Value) -> Result<Value, String> {
    let mut body = body;
    let object = body
        .as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;

    if !object.get("messages").is_some_and(Value::is_array) {
        return Err("messages must be an array".to_string());
    }
    if object.contains_key("mcp_servers") {
        return Err(
            "mcp_servers is unsupported; only client function tools are supported".to_string(),
        );
    }
    validate_anthropic_cache_controls(object)?;

    // Server-side fallbacks can bill several models in one response, which the
    // gateway's single-model settlement record cannot represent. Rejecting
    // them here makes the failure deterministic and, critically, happens before
    // either native or Worker admission creates a cost-cap reservation.
    if object.contains_key("fallbacks") {
        return Err(
            "fallbacks is unsupported because NovaGuard cannot yet meter mixed-model Anthropic usage"
                .to_string(),
        );
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| "model must be a non-empty string".to_string())?
        .to_string();
    if let Some(speed) = object.get("speed") {
        match speed.as_str() {
            Some("standard") => {}
            Some("fast")
                if anthropic_model_is(&model, "claude-opus-5")
                    || anthropic_model_is(&model, "claude-opus-4-8") => {}
            Some("fast") => {
                return Err(format!("speed=fast is unsupported for model {model}"));
            }
            _ => return Err("speed must be standard or fast".to_string()),
        }
    }
    if let Some(inference_geo) = object.get("inference_geo") {
        let inference_geo = inference_geo
            .as_str()
            .filter(|geo| matches!(*geo, "global" | "us"))
            .ok_or_else(|| "inference_geo must be either global or us".to_string())?;
        if !crate::policy::pricing::anthropic_supports_inference_geo(&model) {
            return Err(format!(
                "inference_geo={inference_geo} is unsupported for model {model}"
            ));
        }
    }
    if anthropic_has_fixed_sampling(&model) {
        // The API reference documents temperature=1 and top_p>=0.99 as the
        // backwards-compatible defaults; top_k is rejected whenever present on
        // post-4.6 models:
        // <https://platform.claude.com/docs/en/api/messages/create>
        if object
            .get("temperature")
            .is_some_and(|value| value.as_f64() != Some(1.0))
        {
            return Err(format!(
                "temperature is unsupported at non-default values for model {model}; omit it or use 1"
            ));
        }
        if object.get("top_p").is_some_and(|value| {
            !value
                .as_f64()
                .is_some_and(|top_p| (0.99..=1.0).contains(&top_p))
        }) {
            return Err(format!(
                "top_p is unsupported at non-default values for model {model}; omit it or use a value from 0.99 through 1"
            ));
        }
        if object.contains_key("top_k") {
            return Err(format!("top_k is unsupported for model {model}; omit it"));
        }
    }

    if anthropic_model_is(&model, "claude-sonnet-5") {
        // Sonnet 5 removed manual extended thinking and, like Sonnet 4.6, does
        // not accept a final assistant prefill:
        // <https://platform.claude.com/docs/en/about-claude/models/whats-new-sonnet-5>
        if object
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled")
        {
            return Err(
                "thinking.type=enabled is unsupported for claude-sonnet-5; use adaptive or disabled thinking"
                    .to_string(),
            );
        }
        let final_provider_role =
            object
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|messages| {
                    messages.iter().rev().find_map(|message| {
                        let role = message.get("role").and_then(Value::as_str)?;
                        (!matches!(role, "system" | "developer")).then_some(role)
                    })
                });
        if final_provider_role == Some("assistant") {
            return Err(
                "final assistant message prefilling is unsupported for claude-sonnet-5".to_string(),
            );
        }
    }

    let mut max_tokens = None;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let Some(value) = object.get(key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        max_tokens = Some(
            value
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("{key} must be a positive integer"))?,
        );
        break;
    }
    let max_tokens = max_tokens.unwrap_or_else(crate::policy::pricing::assumed_output_tokens);

    if let Some(n) = object.get("n") {
        if n.as_u64() != Some(1) {
            return Err("n must be 1 for Anthropic requests".to_string());
        }
    }
    let disable_parallel_tool_use = match object.get("parallel_tool_calls") {
        Some(Value::Bool(parallel)) => !parallel,
        Some(_) => return Err("parallel_tool_calls must be a boolean".to_string()),
        None => false,
    };
    if object.contains_key("functions") || object.contains_key("function_call") {
        return Err(
            "legacy functions/function_call are unsupported; use tools/tool_choice".to_string(),
        );
    }
    if let Some(temperature) = object.get("temperature") {
        let temperature = temperature
            .as_f64()
            .ok_or_else(|| "temperature must be a number".to_string())?;
        if temperature < 0.0 {
            return Err("temperature must be non-negative".to_string());
        }
        if temperature > 1.0 {
            object.insert("temperature".to_string(), json!(1.0));
        }
    }

    if let Some(system) = object.get("system") {
        let valid = match system {
            Value::String(_) => true,
            Value::Array(blocks) => blocks.iter().all(|block| {
                block.get("type").and_then(Value::as_str) == Some("text")
                    && block.get("text").and_then(Value::as_str).is_some()
            }),
            _ => false,
        };
        if !valid {
            return Err("system must be a string or an array of text blocks".to_string());
        }
    }

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages was validated as an array");
    let mut pending_tool_ids: Vec<String> = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let message = message
            .as_object()
            .ok_or_else(|| format!("messages[{message_index}] must be an object"))?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("messages[{message_index}].role must be a string"))?;
        if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
            return Err(format!(
                "messages[{message_index}].role is unsupported: {role}"
            ));
        }
        if !pending_tool_ids.is_empty() && role != "tool" {
            return Err(format!(
                "messages[{message_index}] must return every pending tool result before the next message"
            ));
        }

        let content_is_text = |content: Option<&Value>| match content {
            Some(Value::String(_)) => true,
            Some(Value::Array(parts)) => parts.iter().all(|part| {
                part.get("type").and_then(Value::as_str) == Some("text")
                    && part.get("text").and_then(Value::as_str).is_some()
            }),
            _ => false,
        };
        if matches!(role, "system" | "developer") && !content_is_text(message.get("content")) {
            return Err(format!(
                "messages[{message_index}].content must be a string or an array of text blocks"
            ));
        }

        if role == "tool" {
            if message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .is_none()
            {
                return Err(format!(
                    "messages[{message_index}].tool_call_id must be a non-empty string"
                ));
            }
            if !matches!(
                message.get("content"),
                Some(Value::String(_) | Value::Array(_))
            ) {
                return Err(format!(
                    "messages[{message_index}].content must be a string or an array"
                ));
            }
            if pending_tool_ids.is_empty() {
                return Err(format!(
                    "messages[{message_index}] is a tool result without a preceding assistant tool call"
                ));
            }
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .expect("tool_call_id was validated");
            let Some(position) = pending_tool_ids
                .iter()
                .position(|pending| pending == tool_call_id)
            else {
                return Err(format!(
                    "messages[{message_index}].tool_call_id does not match a pending assistant tool call: {tool_call_id}"
                ));
            };
            pending_tool_ids.remove(position);
        }

        if message.get("function_call").is_some() {
            return Err(format!(
                "messages[{message_index}].function_call is unsupported; use tool_calls"
            ));
        }
        if message.get("tool_calls").is_some() && role != "assistant" {
            return Err(format!(
                "messages[{message_index}].tool_calls is only valid for assistant messages"
            ));
        }

        if let Some(tool_calls) = message.get("tool_calls") {
            let tool_calls = tool_calls
                .as_array()
                .ok_or_else(|| format!("messages[{message_index}].tool_calls must be an array"))?;
            if tool_calls.is_empty() {
                return Err(format!(
                    "messages[{message_index}].tool_calls must not be empty"
                ));
            }
            for (tool_index, tool_call) in tool_calls.iter().enumerate() {
                let prefix = format!("messages[{message_index}].tool_calls[{tool_index}]");
                let tool_call = tool_call
                    .as_object()
                    .ok_or_else(|| format!("{prefix} must be an object"))?;
                if tool_call.get("type").and_then(Value::as_str) != Some("function") {
                    return Err(format!("{prefix}.type must be function"));
                }
                if tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .is_none()
                {
                    return Err(format!("{prefix}.id must be a non-empty string"));
                }
                let function = tool_call
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| format!("{prefix}.function must be an object"))?;
                if function
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .is_none()
                {
                    return Err(format!("{prefix}.function.name must be a non-empty string"));
                }
                let arguments_are_object = match function.get("arguments") {
                    Some(Value::String(arguments)) => serde_json::from_str::<Value>(arguments)
                        .ok()
                        .is_some_and(|arguments| arguments.is_object()),
                    Some(Value::Object(_)) => true,
                    _ => false,
                };
                if !arguments_are_object {
                    return Err(format!(
                        "{prefix}.function.arguments must encode a JSON object"
                    ));
                }
                pending_tool_ids.push(
                    tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .expect("tool-call id was validated")
                        .to_string(),
                );
            }
        }
    }
    if !pending_tool_ids.is_empty() {
        return Err("request ends before every pending tool result is returned".to_string());
    }

    object.insert("max_tokens".to_string(), json!(max_tokens));

    let mut hoisted_system = Vec::new();
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        messages.retain(|message| {
            let role = message.get("role").and_then(Value::as_str);
            if !matches!(role, Some("system" | "developer")) {
                return true;
            }

            let text = match message.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>(),
                _ => String::new(),
            };
            hoisted_system.push(text);
            false
        });
    }

    if !hoisted_system.is_empty() {
        match object.get_mut("system") {
            Some(Value::Array(blocks)) => {
                blocks.extend(
                    hoisted_system
                        .into_iter()
                        .map(|text| json!({"type": "text", "text": text})),
                );
            }
            Some(Value::String(existing)) => {
                let mut all = Vec::with_capacity(hoisted_system.len() + 1);
                all.push(std::mem::take(existing));
                all.extend(hoisted_system);
                *existing = all.join("\n");
            }
            None => {
                object.insert(
                    "system".to_string(),
                    Value::String(hoisted_system.join("\n")),
                );
            }
            Some(_) => {}
        }
    }

    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        let original = std::mem::take(messages);
        let mut converted = Vec::with_capacity(original.len());
        let mut previous_was_tool_result = false;

        for (message_index, mut message) in original.into_iter().enumerate() {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let has_tool_calls = message.get("tool_calls").is_some();
            if let Some(map) = message.as_object_mut() {
                for field in ["name", "refusal", "audio"] {
                    map.remove(field);
                }
                let content = map.remove("content").unwrap_or(Value::Null);
                let content = normalize_anthropic_message_content(
                    content,
                    &role,
                    message_index,
                    role == "assistant" && has_tool_calls,
                )?;
                map.insert("content".to_string(), content);
            }

            if role == "assistant" && has_tool_calls {
                let mut content = match message.get("content") {
                    Some(Value::String(text)) if !text.is_empty() => {
                        vec![json!({"type": "text", "text": text})]
                    }
                    Some(Value::Array(blocks)) => blocks.clone(),
                    _ => Vec::new(),
                };
                if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for tool_call in tool_calls {
                        let function = tool_call.get("function").unwrap_or(&Value::Null);
                        let input = match function.get("arguments") {
                            Some(Value::String(arguments)) => {
                                serde_json::from_str(arguments).unwrap_or(Value::Null)
                            }
                            Some(arguments) => arguments.clone(),
                            None => Value::Object(serde_json::Map::new()),
                        };
                        content.push(json!({
                            "type": "tool_use",
                            "id": tool_call.get("id").unwrap_or(&Value::Null),
                            "name": function.get("name").unwrap_or(&Value::Null),
                            "input": input,
                        }));
                    }
                }
                if let Some(map) = message.as_object_mut() {
                    map.remove("tool_calls");
                    map.insert("content".to_string(), Value::Array(content));
                }
                converted.push(message);
                previous_was_tool_result = false;
                continue;
            }

            if role == "tool" {
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": message.get("tool_call_id").unwrap_or(&Value::Null),
                    "content": message.get("content").cloned().unwrap_or(Value::String(String::new())),
                });
                if previous_was_tool_result {
                    if let Some(content) = converted
                        .last_mut()
                        .and_then(|message: &mut Value| message.get_mut("content"))
                        .and_then(Value::as_array_mut)
                    {
                        content.push(result);
                    }
                } else {
                    converted.push(json!({"role": "user", "content": [result]}));
                }
                previous_was_tool_result = true;
                continue;
            }

            converted.push(message);
            previous_was_tool_result = false;
        }

        *messages = converted;
    }

    if let Some(stop_sequences) = object.get("stop_sequences") {
        if !stop_sequences
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err("stop_sequences must be an array of strings".to_string());
        }
    }
    if let Some(stop) = object.get("stop") {
        let valid = stop.is_string()
            || stop
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string));
        if !valid {
            return Err("stop must be a string or an array of strings".to_string());
        }
    }
    if let Some(stop) = object.remove("stop") {
        if !object.contains_key("stop_sequences") {
            let sequences = match stop {
                Value::String(sequence) => Value::Array(vec![Value::String(sequence)]),
                Value::Array(sequences) => Value::Array(sequences),
                _ => unreachable!("stop was validated above"),
            };
            if !sequences.is_null() {
                object.insert("stop_sequences".to_string(), sequences);
            }
        }
    }

    if let Some(tools) = object.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| "tools must be an array".to_string())?;
        for (tool_index, tool) in tools.iter().enumerate() {
            let prefix = format!("tools[{tool_index}]");
            let tool = tool
                .as_object()
                .ok_or_else(|| format!("{prefix} must be an object"))?;
            match tool.get("type") {
                Some(Value::String(kind)) if kind == "function" => {
                    let function = tool
                        .get("function")
                        .and_then(Value::as_object)
                        .ok_or_else(|| format!("{prefix}.function must be an object"))?;
                    if function
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .is_none()
                    {
                        return Err(format!("{prefix}.function.name must be a non-empty string"));
                    }
                    if function
                        .get("description")
                        .is_some_and(|description| !description.is_string())
                    {
                        return Err(format!("{prefix}.function.description must be a string"));
                    }
                    if function
                        .get("parameters")
                        .is_some_and(|parameters| !parameters.is_object())
                    {
                        return Err(format!("{prefix}.function.parameters must be an object"));
                    }
                }
                Some(Value::String(kind)) => {
                    return Err(format!(
                        "{prefix}.type `{kind}` is unsupported; only client function tools are supported"
                    ));
                }
                Some(_) => {
                    return Err(format!(
                        "{prefix}.type must be `function` or omitted for a native client tool"
                    ));
                }
                None => {
                    if tool
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .is_none()
                    {
                        return Err(format!("{prefix}.name must be a non-empty string"));
                    }
                    if !tool.get("input_schema").is_some_and(Value::is_object) {
                        return Err(format!("{prefix}.input_schema must be an object"));
                    }
                }
            }
        }
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let Some(function) = tool.get("function").and_then(Value::as_object) else {
                continue;
            };
            let mut mapped = serde_json::Map::new();
            if let Some(name) = function.get("name") {
                mapped.insert("name".to_string(), name.clone());
            }
            if let Some(description) = function.get("description") {
                mapped.insert("description".to_string(), description.clone());
            }
            mapped.insert(
                "input_schema".to_string(),
                function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            *tool = Value::Object(mapped);
        }
    }

    if let Some(choice) = object.remove("tool_choice") {
        let mapped = match choice {
            Value::String(choice) if choice == "auto" => Some(json!({"type": "auto"})),
            Value::String(choice) if choice == "required" => Some(json!({"type": "any"})),
            Value::String(choice) if choice == "none" => Some(json!({"type": "none"})),
            Value::Object(choice)
                if choice.get("type").and_then(Value::as_str) == Some("function") =>
            {
                let name = choice
                    .get("function")
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        "tool_choice.function.name must be a non-empty string".to_string()
                    })?;
                Some(json!({"type": "tool", "name": name}))
            }
            Value::String(choice) => {
                return Err(format!("unsupported tool_choice: {choice}"));
            }
            other => Some(other),
        };
        if let Some(mapped) = mapped {
            object.insert("tool_choice".to_string(), mapped);
        }
    }

    if disable_parallel_tool_use
        && object
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
    {
        match object.get_mut("tool_choice") {
            Some(Value::Object(choice))
                if choice.get("type").and_then(Value::as_str) != Some("none") =>
            {
                choice.insert("disable_parallel_tool_use".to_string(), Value::Bool(true));
            }
            None => {
                object.insert(
                    "tool_choice".to_string(),
                    json!({"type": "auto", "disable_parallel_tool_use": true}),
                );
            }
            _ => {}
        }
    }

    for field in [
        "max_completion_tokens",
        "max_output_tokens",
        "stream_options",
        "response_format",
        "frequency_penalty",
        "presence_penalty",
        "logprobs",
        "top_logprobs",
        "seed",
        "service_tier",
        "parallel_tool_calls",
        "reasoning_effort",
        "store",
        "user",
        "modalities",
        "audio",
        "prediction",
        "logit_bias",
        "web_search_options",
        "n",
        "functions",
        "function_call",
        "verbosity",
        "prompt_cache_key",
        "safety_identifier",
        "metadata",
    ] {
        object.remove(field);
    }

    Ok(body)
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

    let max_tokens = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .into_iter()
        .find_map(|field| body.get(field).and_then(Value::as_u64))
        .unwrap_or(1000);
    let mut inference = json!({
        "maxTokens": max_tokens,
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

    let usage = resp
        .get("usage")
        .and_then(Value::as_object)
        .map(|reported| {
            let mut converted = serde_json::Map::new();
            for (source, target) in [
                ("inputTokens", "prompt_tokens"),
                ("outputTokens", "completion_tokens"),
                ("totalTokens", "total_tokens"),
            ] {
                if let Some(tokens) = reported.get(source).and_then(Value::as_u64) {
                    converted.insert(target.to_string(), json!(tokens));
                }
            }
            Value::Object(converted)
        })
        .unwrap_or(Value::Null);

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
        "usage": usage
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_auth_parser_is_case_insensitive_and_rejects_other_schemes() {
        for (raw, expected) in [
            ("Bearer sk-ant-1", Some("sk-ant-1")),
            ("bearer   sk-ant-2  ", Some("sk-ant-2")),
            ("BEARER sk-ant-3", Some("sk-ant-3")),
            ("Basic c2VjcmV0", None),
            ("Token secret", None),
            ("Bearer", None),
            ("Bearer   ", None),
            ("Bearer key extra", None),
            ("", None),
        ] {
            assert_eq!(authorization_bearer_token(raw), expected, "{raw:?}");
        }
    }

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
    fn strict_openai_rejects_regional_or_unpriced_base_url_overrides() {
        for allowed in [
            None,
            Some("https://api.openai.com"),
            Some("http://127.0.0.1:18899"),
            Some("http://localhost:18899"),
        ] {
            assert!(
                validate_strict_openai_base_url(allowed).is_ok(),
                "{allowed:?}"
            );
        }
        for unsupported in [
            "https://eu.api.openai.com",
            "https://us.api.openai.com/v1",
            "https://compatible.example.com/v1",
        ] {
            let error = validate_strict_openai_base_url(Some(unsupported)).unwrap_err();
            assert!(error.contains("OpenAI base URL"), "{unsupported}: {error}");
        }
    }

    #[test]
    fn strict_anthropic_rejects_unpriced_base_url_overrides() {
        for allowed in [
            None,
            Some("https://api.anthropic.com"),
            Some("http://127.0.0.1:18899"),
        ] {
            assert!(
                validate_strict_anthropic_base_url(allowed).is_ok(),
                "{allowed:?}"
            );
        }
        let error =
            validate_strict_anthropic_base_url(Some("https://anthropic-compatible.example.com"))
                .unwrap_err();
        assert!(error.contains("Anthropic base URL"), "{error}");
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
    fn strict_admission_counts_the_full_forward_body_and_tool_overhead() {
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "x".repeat(8_000),
                    "parameters": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}}
                    }
                }
            }]
        });
        let serialized = serde_json::to_vec(&body).unwrap().len() as u32;
        let advisory = estimate_admission_input_tokens(&body, false);
        let strict = estimate_admission_input_tokens(&body, true);
        assert_eq!(advisory, 1, "legacy advisory estimate remains text-only");
        assert_eq!(
            strict,
            serialized.saturating_add(STRICT_TOOL_PROMPT_RESERVE_TOKENS)
        );
        assert!(strict > advisory);

        let legacy_functions = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "functions": [{"name": "lookup", "parameters": {"type": "object"}}]
        });
        assert_eq!(
            estimate_admission_input_tokens(&legacy_functions, true),
            serde_json::to_vec(&legacy_functions).unwrap().len() as u32
                + STRICT_TOOL_PROMPT_RESERVE_TOKENS
        );
    }

    #[test]
    fn strict_admission_rejects_opaque_provider_inputs_but_allows_bounded_data() {
        for (body, expected) in [
            (
                json!({"mcp_servers": [{"type": "url", "url": "https://mcp.example"}]}),
                "mcp_servers",
            ),
            (
                json!({"web_search_options": {"search_context_size": "high"}}),
                "web_search_options",
            ),
            (
                json!({"messages": [{"role": "user", "content": [{
                    "type": "image_url",
                    "image_url": {"url": "https://example.com/large.png"}
                }]}]}),
                "remote image",
            ),
            (
                json!({"input": [{"type": "input_file", "file_id": "file_123"}]}),
                "file or document input",
            ),
            (
                json!({"messages": [{"role": "user", "content": [{
                    "type": "input_audio",
                    "input_audio": {"data": "AAAA", "format": "wav"}
                }]}]}),
                "audio input",
            ),
            (
                json!({"tools": [{"type": "web_search_preview"}]}),
                "server-side tool",
            ),
        ] {
            let error = validate_strict_admission_input(&body).unwrap_err();
            assert!(error.contains(expected), "{error:?}");
        }

        for body in [
            json!({"messages": [{"role": "user", "content": "https://example.com is plain text"}]}),
            json!({"tools": [{"type": "function", "function": {
                "name": "lookup", "parameters": {"type": "object"}
            }}]}),
        ] {
            validate_strict_admission_input(&body).unwrap();
        }

        let inline_image = json!({"messages": [{"role": "user", "content": [{
            "type": "image_url",
            "image_url": {"url": "data:image/png;base64,AAAA"}
        }]}]});
        assert!(validate_strict_admission_input(&inline_image)
            .unwrap_err()
            .contains("image input"));
    }

    #[test]
    fn strict_preflight_rejects_xai_server_side_search() {
        for provider in ["xai", "grok"] {
            let mut request = json!({
                "model": "grok-4.3",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "latest news"}],
                "search_parameters": {"mode": "on"}
            });

            let error =
                prepare_strict_admission_body(provider, "/v1/chat/completions", &mut request)
                    .unwrap_err();
            assert!(error.contains("search_parameters"), "{provider}: {error:?}");
        }
    }

    #[test]
    fn strict_preflight_rejects_bedrock_models_without_region_aware_pricing() {
        for model in ["amazon.nova-pro-v1:0", "amazon.titan-text-express-v1"] {
            let mut request = json!({
                "model": model,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            });

            let error =
                prepare_strict_admission_body("bedrock", "/v1/chat/completions", &mut request)
                    .unwrap_err();
            assert!(error.contains("region-aware pricing"), "{model}: {error:?}");
        }

        let mut catalogued_claude = json!({
            "model": "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        });
        prepare_strict_admission_body("bedrock", "/v1/chat/completions", &mut catalogued_claude)
            .unwrap();
    }

    #[test]
    fn strict_preflight_scopes_paths_providers_and_paid_request_modes() {
        let body = || {
            json!({
                "model": "gpt-4o",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            })
        };
        for (provider, path, expected) in [
            ("openai", "/v1/responses", "/v1/chat/completions"),
            ("perplexity", "/v1/chat/completions", "Perplexity"),
            ("openrouter", "/v1/chat/completions", "OpenRouter"),
        ] {
            let mut request = body();
            let error = prepare_strict_admission_body(provider, path, &mut request).unwrap_err();
            assert!(error.contains(expected), "{error:?}");
        }

        for model in ["groq/compound", "groq/compound-mini"] {
            let mut request = body();
            request["model"] = json!(model);
            let error = prepare_strict_admission_body("groq", "/v1/chat/completions", &mut request)
                .unwrap_err();
            assert!(error.contains("Compound"), "{model}: {error:?}");
        }

        let mut gemini_search = body();
        gemini_search["model"] = json!("gemini-2.5-flash");
        gemini_search["tools"] = json!([{"google_search": {}}]);
        let error =
            prepare_strict_admission_body("gemini", "/v1/chat/completions", &mut gemini_search)
                .unwrap_err();
        assert!(error.contains("type=function"), "{error:?}");

        let mut gemini_cached_context = body();
        gemini_cached_context["model"] = json!("gemini-2.5-flash");
        gemini_cached_context["extra_body"] =
            json!({"google": {"cached_content": "cachedContents/abc"}});
        let error = prepare_strict_admission_body(
            "google",
            "/v1/chat/completions",
            &mut gemini_cached_context,
        )
        .unwrap_err();
        assert!(error.contains("cached context"), "{error:?}");

        let mut mistral_document = body();
        mistral_document["model"] = json!("mistral-medium-latest");
        mistral_document["messages"][0]["content"] = json!([{
            "type": "document_url",
            "document_url": "https://example.com/large.pdf"
        }]);
        let error =
            prepare_strict_admission_body("mistral", "/v1/chat/completions", &mut mistral_document)
                .unwrap_err();
        assert!(error.contains("document"), "{error:?}");

        let mut together_video = body();
        together_video["model"] = json!("Qwen/Qwen2.5-VL-72B-Instruct");
        together_video["messages"][0]["content"] = json!([{
            "type": "video_url",
            "video_url": {"url": "https://example.com/large.mp4"}
        }]);
        let error =
            prepare_strict_admission_body("together", "/v1/chat/completions", &mut together_video)
                .unwrap_err();
        assert!(error.contains("video"), "{error:?}");

        let mut native_bedrock = body();
        native_bedrock["model"] = json!("global.anthropic.claude-sonnet-4-5-20250929-v1:0");
        native_bedrock["inferenceConfig"] = json!({"maxTokens": 100_000});
        let error =
            prepare_strict_admission_body("bedrock", "/v1/chat/completions", &mut native_bedrock)
                .unwrap_err();
        assert!(error.contains("provider-native Bedrock"), "{error:?}");

        let mut bedrock_tools = body();
        bedrock_tools["model"] = json!("global.anthropic.claude-sonnet-4-5-20250929-v1:0");
        bedrock_tools["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "lookup",
                "parameters": {"type": "object"}
            }
        }]);
        let error =
            prepare_strict_admission_body("bedrock", "/v1/chat/completions", &mut bedrock_tools)
                .unwrap_err();
        assert!(error.contains("Bedrock tool use"), "{error:?}");

        for (field, value, expected) in [
            ("service_tier", json!("priority"), "service_tier"),
            ("n", json!(2), "n must be 1"),
            ("best_of", json!(2), "best_of must be 1"),
            (
                "prompt_cache_retention",
                json!("24h"),
                "prompt_cache_retention",
            ),
        ] {
            let mut request = body();
            request[field] = value;
            let error =
                prepare_strict_admission_body("openai", "/v1/chat/completions", &mut request)
                    .unwrap_err();
            assert!(error.contains(expected), "{error:?}");
        }

        let mut openai = body();
        let tokens =
            prepare_strict_admission_body("openai", "/v1/chat/completions", &mut openai).unwrap();
        assert_eq!(openai["service_tier"], "default");
        assert_eq!(tokens, serde_json::to_vec(&openai).unwrap().len() as u32);

        let mut implicit_cache = body();
        implicit_cache["model"] = json!("gpt-5.6-sol");
        prepare_strict_admission_body("openai", "/v1/chat/completions", &mut implicit_cache)
            .unwrap();
        assert_eq!(implicit_cache["prompt_cache_options"]["mode"], "explicit");

        let mut requested_implicit_cache = body();
        requested_implicit_cache["model"] = json!("gpt-5.6-sol");
        requested_implicit_cache["prompt_cache_options"] = json!({"mode": "implicit"});
        let error = prepare_strict_admission_body(
            "openai",
            "/v1/chat/completions",
            &mut requested_implicit_cache,
        )
        .unwrap_err();
        assert!(error.contains("prompt cache"), "{error:?}");

        let mut explicit_breakpoint = body();
        explicit_breakpoint["model"] = json!("gpt-5.6-sol");
        explicit_breakpoint["messages"][0]["content"] = json!([{
            "type": "text",
            "text": "hi",
            "prompt_cache_breakpoint": {}
        }]);
        let error = prepare_strict_admission_body(
            "openai",
            "/v1/chat/completions",
            &mut explicit_breakpoint,
        )
        .unwrap_err();
        assert!(error.contains("prompt_cache_breakpoint"), "{error:?}");

        let mut output_alias = body();
        output_alias.as_object_mut().unwrap().remove("max_tokens");
        output_alias["max_output_tokens"] = json!(64);
        prepare_strict_admission_body("openai", "/v1/chat/completions", &mut output_alias).unwrap();
        assert_eq!(output_alias["max_completion_tokens"], 64);
        assert!(output_alias.get("max_output_tokens").is_none());

        let mut null_native_limit = body();
        null_native_limit["max_tokens"] = Value::Null;
        null_native_limit["max_completion_tokens"] = Value::Null;
        null_native_limit["max_output_tokens"] = json!(64);
        prepare_strict_admission_body("openai", "/v1/chat/completions", &mut null_native_limit)
            .unwrap();
        assert!(null_native_limit.get("max_tokens").is_none());
        assert_eq!(null_native_limit["max_completion_tokens"], 64);
        assert!(null_native_limit.get("max_output_tokens").is_none());

        let mut compatible = body();
        prepare_strict_admission_body("groq", "/v1/chat/completions", &mut compatible).unwrap();
        assert!(compatible.get("service_tier").is_none());
    }

    #[test]
    fn strict_preflight_normalizes_the_generic_output_limit_for_compatible_providers() {
        for provider in [
            "groq",
            "together",
            "fireworks",
            "mistral",
            "deepseek",
            "xai",
            "grok",
            "google",
            "gemini",
            "cohere",
        ] {
            let mut request = json!({
                "model": "bounded-chat-model",
                "max_output_tokens": 137,
                "messages": [{"role": "user", "content": "hi"}]
            });
            let admitted_limit = crate::policy::worker_remote::resolve_max_output_tokens(&request)
                .unwrap()
                .unwrap();

            prepare_strict_admission_body(provider, "/v1/chat/completions", &mut request).unwrap();

            assert_eq!(admitted_limit, 137, "{provider}");
            assert_eq!(
                request.get("max_tokens").and_then(Value::as_u64),
                Some(admitted_limit),
                "{provider}: the forwarded ceiling must equal the admitted ceiling"
            );
            assert!(
                request.get("max_completion_tokens").is_none(),
                "{provider}: only one provider-native ceiling may be forwarded"
            );
            assert!(
                request.get("max_output_tokens").is_none(),
                "{provider}: the gateway-only alias must not reach the provider"
            );
        }
    }

    #[test]
    fn strict_preflight_collapses_equal_output_aliases() {
        let mut equal = json!({
            "model": "bounded-chat-model",
            "max_tokens": 137,
            "max_completion_tokens": 137,
            "max_output_tokens": 137,
            "messages": [{"role": "user", "content": "hi"}]
        });
        prepare_strict_admission_body("groq", "/v1/chat/completions", &mut equal).unwrap();
        assert_eq!(equal.get("max_tokens").and_then(Value::as_u64), Some(137));
        assert!(equal.get("max_completion_tokens").is_none());
        assert!(equal.get("max_output_tokens").is_none());
    }

    #[test]
    fn strict_preflight_rejects_divergent_output_aliases() {
        for provider in ["openai", "groq", "anthropic", "bedrock"] {
            let model = match provider {
                "anthropic" => "claude-sonnet-5",
                "bedrock" => "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                _ => "bounded-chat-model",
            };
            let mut divergent = json!({
                "model": model,
                "max_tokens": 64,
                "max_output_tokens": 65,
                "messages": [{"role": "user", "content": "hi"}]
            });

            let error =
                prepare_strict_admission_body(provider, "/v1/chat/completions", &mut divergent)
                    .unwrap_err();
            assert!(error.contains("must agree"), "{provider}: {error:?}");
        }
    }

    #[test]
    fn strict_preflight_keeps_anthropic_and_bedrock_output_alias_conversion_bounded() {
        let mut anthropic = json!({
            "model": "claude-sonnet-5",
            "max_output_tokens": 83,
            "messages": [{"role": "user", "content": "hi"}]
        });
        prepare_strict_admission_body("anthropic", "/v1/chat/completions", &mut anthropic).unwrap();
        assert_eq!(
            openai_to_anthropic_messages(anthropic).unwrap()["max_tokens"],
            83
        );

        let mut bedrock = json!({
            "model": "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "max_output_tokens": 91,
            "messages": [{"role": "user", "content": "hi"}]
        });
        prepare_strict_admission_body("bedrock", "/v1/chat/completions", &mut bedrock).unwrap();
        assert_eq!(
            openai_to_bedrock_converse(&bedrock)["inferenceConfig"]["maxTokens"],
            91
        );
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
    fn anthropic_usage_totals_saturate_instead_of_overflowing() {
        let out = transform_anthropic_to_openai_format(
            json!({
                "id": "msg_overflow",
                "model": "claude-sonnet-5",
                "content": [],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": u64::MAX, "output_tokens": 1}
            }),
            1,
        );
        assert_eq!(out["usage"]["total_tokens"], u64::MAX);
    }

    #[test]
    fn anthropic_to_openai_maps_buffered_tool_use_and_finish_reasons() {
        let anthropic = json!({
            "id": "msg_tools",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [
                {"type": "text", "text": "I will check both cities."},
                {
                    "type": "tool_use",
                    "id": "toolu_delhi",
                    "name": "lookup_weather",
                    "input": {"city": "Delhi"}
                },
                {
                    "type": "tool_use",
                    "id": "toolu_mumbai",
                    "name": "lookup_weather",
                    "input": {"city": "Mumbai", "units": "celsius"}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 20,
                "output_tokens": 12,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 25,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 20,
                    "ephemeral_1h_input_tokens": 5
                },
                "server_tool_use": {"web_search_requests": 2},
                "inference_geo": "us",
                "speed": "fast"
            }
        });

        let out = transform_anthropic_to_openai_format(anthropic, 1_700_000_001);
        assert_eq!(
            out["choices"][0]["message"]["content"],
            "I will check both cities."
        );
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"],
            json!([
                {
                    "id": "toolu_delhi",
                    "type": "function",
                    "function": {
                        "name": "lookup_weather",
                        "arguments": "{\"city\":\"Delhi\"}"
                    }
                },
                {
                    "id": "toolu_mumbai",
                    "type": "function",
                    "function": {
                        "name": "lookup_weather",
                        "arguments": "{\"city\":\"Mumbai\",\"units\":\"celsius\"}"
                    }
                }
            ])
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["usage"]["total_tokens"], 32);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 100);
        assert_eq!(out["usage"]["cache_creation_input_tokens"], 25);
        assert_eq!(
            out["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
            5
        );
        assert_eq!(out["usage"]["server_tool_use"]["web_search_requests"], 2);
        assert_eq!(out["usage"]["inference_geo"], "us");
        assert_eq!(out["usage"]["speed"], "fast");

        let tool_only = transform_anthropic_to_openai_format(
            json!({
                "id": "msg_tool_only",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_only",
                    "name": "lookup",
                    "input": {}
                }],
                "stop_reason": "model_context_window_exceeded"
            }),
            1_700_000_002,
        );
        assert!(tool_only["choices"][0]["message"]["content"].is_null());
        assert_eq!(tool_only["choices"][0]["finish_reason"], "length");

        let refusal = transform_anthropic_to_openai_format(
            json!({
                "id": "msg_refusal",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "I cannot help with that."}],
                "stop_reason": "refusal"
            }),
            1_700_000_003,
        );
        assert_eq!(refusal["choices"][0]["finish_reason"], "content_filter");
    }

    #[test]
    fn anthropic_buffered_pre_output_refusal_is_marked_unbilled() {
        let refusal = transform_anthropic_to_openai_format(
            json!({
                "id": "msg_refusal",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "I cannot help with that."}],
                "stop_reason": "refusal",
                "usage": {"input_tokens": 100, "output_tokens": 0}
            }),
            1,
        );
        assert_eq!(refusal["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(refusal["usage"]["unbilled_refusal"], true);

        let partial = transform_anthropic_to_openai_format(
            json!({
                "id": "msg_partial_refusal",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "partial"}],
                "stop_reason": "refusal",
                "usage": {"input_tokens": 100, "output_tokens": 1}
            }),
            1,
        );
        assert!(partial["usage"].get("unbilled_refusal").is_none());
    }

    #[test]
    fn anthropic_request_maps_output_limits_and_removes_openai_only_fields() {
        let converted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_completion_tokens": 77,
            "stream": true,
            "stream_options": {"include_usage": true},
            "response_format": {"type": "json_object"},
            "frequency_penalty": 0.2,
            "presence_penalty": 0.1,
            "logprobs": true,
            "top_logprobs": 3,
            "seed": 42,
            "service_tier": "auto",
            "parallel_tool_calls": true,
            "reasoning_effort": "medium",
            "store": true,
            "user": "customer-1",
            "modalities": ["text"],
            "audio": {"format": "wav"},
            "prediction": {"type": "content", "content": "hello"},
            "logit_bias": {"123": 1},
            "web_search_options": {},
            "n": 1,
            "metadata": {"trace": "openai-only"},
            "verbosity": "low",
            "prompt_cache_key": "cache-key",
            "safety_identifier": "safety-id"
        }))
        .expect("valid OpenAI request converts");

        assert_eq!(converted["max_tokens"], 77);
        assert_eq!(converted["stream"], true);
        for removed in [
            "max_completion_tokens",
            "stream_options",
            "response_format",
            "frequency_penalty",
            "presence_penalty",
            "logprobs",
            "top_logprobs",
            "seed",
            "service_tier",
            "parallel_tool_calls",
            "reasoning_effort",
            "store",
            "user",
            "modalities",
            "audio",
            "prediction",
            "logit_bias",
            "web_search_options",
            "n",
            "metadata",
            "verbosity",
            "prompt_cache_key",
            "safety_identifier",
        ] {
            assert!(
                converted.get(removed).is_none(),
                "{removed} leaked upstream"
            );
        }

        let explicit_wins = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 11,
            "max_completion_tokens": 99
        }))
        .unwrap();
        assert_eq!(explicit_wins["max_tokens"], 11);

        let defaulted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        assert_eq!(
            defaulted["max_tokens"],
            crate::policy::pricing::assumed_output_tokens()
        );

        let responses_alias = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": null,
            "max_output_tokens": 33
        }))
        .unwrap();
        assert_eq!(responses_alias["max_tokens"], 33);
        assert!(responses_alias.get("max_output_tokens").is_none());
    }

    #[test]
    fn anthropic_request_hoists_system_and_developer_messages_in_order() {
        let converted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "system": "native system",
            "messages": [
                {"role": "user", "content": "question one"},
                {"role": "system", "content": "system two"},
                {"role": "assistant", "content": "answer one"},
                {"role": "developer", "content": [
                    {"type": "text", "text": "developer three"}
                ]},
                {"role": "user", "content": "question two"}
            ]
        }))
        .unwrap();

        assert_eq!(
            converted["system"],
            "native system\nsystem two\ndeveloper three"
        );
        assert_eq!(
            converted["messages"],
            json!([
                {"role": "user", "content": "question one"},
                {"role": "assistant", "content": "answer one"},
                {"role": "user", "content": "question two"}
            ])
        );

        let preserves_native_blocks = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "system": [{
                "type": "text",
                "text": "cache me",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [
                {"role": "developer", "content": "then this"},
                {"role": "user", "content": "hello"}
            ]
        }))
        .unwrap();
        assert_eq!(
            preserves_native_blocks["system"],
            json!([
                {
                    "type": "text",
                    "text": "cache me",
                    "cache_control": {"type": "ephemeral"}
                },
                {"type": "text", "text": "then this"}
            ])
        );
    }

    #[test]
    fn anthropic_request_maps_stop_without_overwriting_native_stop_sequences() {
        let from_string = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "stop": "END"
        }))
        .unwrap();
        assert_eq!(from_string["stop_sequences"], json!(["END"]));
        assert!(from_string.get("stop").is_none());

        let from_array = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "stop": ["END", "STOP"]
        }))
        .unwrap();
        assert_eq!(from_array["stop_sequences"], json!(["END", "STOP"]));

        let native_wins = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "stop": "OPENAI",
            "stop_sequences": ["NATIVE"]
        }))
        .unwrap();
        assert_eq!(native_wins["stop_sequences"], json!(["NATIVE"]));
        assert!(native_wins.get("stop").is_none());
    }

    #[test]
    fn anthropic_request_maps_openai_function_tools_and_tool_choice() {
        let request = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup_weather",
                    "description": "Look up the weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    },
                    "strict": true
                }
            }],
            "tool_choice": {
                "type": "function",
                "function": {"name": "lookup_weather"}
            }
        });

        let converted = openai_to_anthropic_messages(request).unwrap();
        assert_eq!(
            converted["tools"],
            json!([{
                "name": "lookup_weather",
                "description": "Look up the weather",
                "input_schema": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }])
        );
        assert_eq!(
            converted["tool_choice"],
            json!({"type": "tool", "name": "lookup_weather"})
        );

        for (openai, anthropic) in [
            (json!("auto"), json!({"type": "auto"})),
            (json!("required"), json!({"type": "any"})),
            (json!("none"), json!({"type": "none"})),
        ] {
            let mut body = json!({
                "model": "claude-sonnet-4-5",
                "messages": [{"role": "user", "content": "weather?"}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "lookup_weather",
                        "parameters": {"type": "object"}
                    }
                }]
            });
            body["tool_choice"] = openai;
            let converted = openai_to_anthropic_messages(body).unwrap();
            assert_eq!(converted["tool_choice"], anthropic);
            assert!(
                converted.get("tools").is_some(),
                "tool definitions must remain available when tool_choice is none"
            );
        }
    }

    #[test]
    fn anthropic_request_maps_parallel_tool_control() {
        let make_body = |tool_choice: Option<Value>, parallel: Value| {
            let mut body = json!({
                "model": "claude-sonnet-4-5",
                "messages": [{"role": "user", "content": "weather?"}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "lookup_weather",
                        "parameters": {"type": "object"}
                    }
                }],
                "parallel_tool_calls": parallel
            });
            if let Some(choice) = tool_choice {
                body["tool_choice"] = choice;
            }
            body
        };

        for (openai, expected) in [
            (
                None,
                json!({"type": "auto", "disable_parallel_tool_use": true}),
            ),
            (
                Some(json!("required")),
                json!({"type": "any", "disable_parallel_tool_use": true}),
            ),
            (
                Some(json!({"type": "function", "function": {"name": "lookup_weather"}})),
                json!({
                    "type": "tool",
                    "name": "lookup_weather",
                    "disable_parallel_tool_use": true
                }),
            ),
        ] {
            let converted = openai_to_anthropic_messages(make_body(openai, json!(false))).unwrap();
            assert_eq!(converted["tool_choice"], expected);
            assert!(converted.get("parallel_tool_calls").is_none());
        }

        let default_parallel = openai_to_anthropic_messages(make_body(None, json!(true))).unwrap();
        assert!(default_parallel.get("tool_choice").is_none());
        assert!(default_parallel.get("parallel_tool_calls").is_none());

        let no_tools = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "parallel_tool_calls": false
        }))
        .unwrap();
        assert!(no_tools.get("tool_choice").is_none());
    }

    #[test]
    fn anthropic_request_maps_openai_images_and_sanitizes_message_fields() {
        let converted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {
                    "role": "user",
                    "name": "customer",
                    "content": [
                        {"type": "text", "text": "Compare these images"},
                        {
                            "type": "image_url",
                            "image_url": {"url": "https://example.com/cat.png", "detail": "high"}
                        },
                        {
                            "type": "image_url",
                            "image_url": {"url": "data:image/jpeg;base64,ZmFrZQ=="}
                        }
                    ]
                },
                {
                    "role": "assistant",
                    "content": "I can compare them.",
                    "refusal": null,
                    "audio": {"id": "audio_1"},
                    "name": "assistant-name"
                }
            ]
        }))
        .unwrap();

        assert_eq!(
            converted["messages"][0],
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Compare these images"},
                    {
                        "type": "image",
                        "source": {"type": "url", "url": "https://example.com/cat.png"}
                    },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "ZmFrZQ=="
                        }
                    }
                ]
            })
        );
        assert_eq!(
            converted["messages"][1],
            json!({"role": "assistant", "content": "I can compare them."})
        );
    }

    #[test]
    fn anthropic_request_validates_single_choice_and_caps_temperature() {
        let converted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "n": 1,
            "metadata": {"trace_id": "trace_1"},
            "temperature": 1.7
        }))
        .unwrap();
        assert!(converted.get("n").is_none());
        assert!(converted.get("metadata").is_none());
        assert_eq!(converted["temperature"], 1.0);
    }

    #[test]
    fn anthropic_request_rejects_non_default_sampling_on_constrained_models() {
        for model in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
        ] {
            for (field, value, expected) in [
                (
                    "temperature",
                    json!(0.7),
                    format!(
                        "temperature is unsupported at non-default values for model {model}; omit it or use 1"
                    ),
                ),
                (
                    "temperature",
                    json!(1.7),
                    format!(
                        "temperature is unsupported at non-default values for model {model}; omit it or use 1"
                    ),
                ),
                (
                    "top_p",
                    json!(0.9),
                    format!(
                        "top_p is unsupported at non-default values for model {model}; omit it or use a value from 0.99 through 1"
                    ),
                ),
                (
                    "top_k",
                    json!(40),
                    format!("top_k is unsupported for model {model}; omit it"),
                ),
            ] {
                let mut body = json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hello"}],
                });
                body[field] = value;
                assert_eq!(
                    openai_to_anthropic_messages(body).unwrap_err(),
                    expected,
                    "{field} was accepted for constrained model {model}"
                );
            }
        }
    }

    #[test]
    fn anthropic_request_accepts_default_sampling_on_constrained_models() {
        for model in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
        ] {
            for top_p in [0.99, 1.0] {
                let converted = openai_to_anthropic_messages(json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hello"}],
                    "temperature": 1.0,
                    "top_p": top_p,
                }))
                .unwrap_or_else(|error| panic!("default sampling failed for {model}: {error}"));
                assert_eq!(converted["temperature"], 1.0);
                assert_eq!(converted["top_p"], top_p);
            }
        }
    }

    #[test]
    fn anthropic_request_preserves_legacy_model_sampling() {
        for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            let converted = openai_to_anthropic_messages(json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "temperature": 0.7,
                "top_p": 0.9,
                "top_k": 40,
            }))
            .unwrap();
            assert_eq!(converted["temperature"], 0.7);
            assert_eq!(converted["top_p"], 0.9);
            assert_eq!(converted["top_k"], 40);
        }
    }

    #[test]
    fn anthropic_request_rejects_sonnet_5_manual_thinking() {
        let error = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
        }))
        .unwrap_err();
        assert_eq!(
            error,
            "thinking.type=enabled is unsupported for claude-sonnet-5; use adaptive or disabled thinking"
        );

        for thinking_type in ["adaptive", "disabled"] {
            let converted = openai_to_anthropic_messages(json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "thinking": {"type": thinking_type},
            }))
            .unwrap();
            assert_eq!(converted["thinking"]["type"], thinking_type);
        }
    }

    #[test]
    fn anthropic_request_rejects_sonnet_5_assistant_prefill() {
        let error = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "user", "content": "Return JSON"},
                {"role": "assistant", "content": "{"},
            ],
        }))
        .unwrap_err();
        assert_eq!(
            error,
            "final assistant message prefilling is unsupported for claude-sonnet-5"
        );

        let legacy = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": "Return JSON"},
                {"role": "assistant", "content": "{"},
            ],
        }))
        .unwrap();
        assert_eq!(legacy["messages"][1]["content"], "{");
    }

    #[test]
    fn anthropic_request_rejects_server_side_fallbacks() {
        for fallbacks in [json!("default"), json!([{"model": "claude-opus-4-8"}])] {
            let error = openai_to_anthropic_messages(json!({
                "model": "claude-fable-5",
                "messages": [{"role": "user", "content": "hello"}],
                "fallbacks": fallbacks,
            }))
            .unwrap_err();
            assert_eq!(
                error,
                "fallbacks is unsupported because NovaGuard cannot yet meter mixed-model Anthropic usage"
            );
        }
    }

    #[test]
    fn anthropic_preflight_rejects_mcp_and_provider_server_tools() {
        let cases = [
            (
                json!({
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "mcp_servers": [{"type": "url", "url": "https://mcp.example"}]
                }),
                "mcp_servers is unsupported; only client function tools are supported",
            ),
            (
                json!({
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": [{
                        "type": "web_search_20250305",
                        "name": "web_search",
                        "input_schema": {"type": "object"}
                    }]
                }),
                "tools[0].type `web_search_20250305` is unsupported; only client function tools are supported",
            ),
            (
                json!({
                    "model": "claude-sonnet-5",
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": [{
                        "type": "mcp",
                        "name": "remote_lookup",
                        "input_schema": {"type": "object"}
                    }]
                }),
                "tools[0].type `mcp` is unsupported; only client function tools are supported",
            ),
        ];

        for (request, expected) in cases {
            assert_eq!(openai_to_anthropic_messages(request).unwrap_err(), expected);
        }

        let native_client_tool = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "lookup",
                "description": "Look up a value",
                "input_schema": {"type": "object", "properties": {}}
            }]
        }))
        .expect("an untyped native Anthropic client tool remains supported");
        assert_eq!(native_client_tool["tools"][0]["name"], "lookup");

        let openai_client_tool = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]
        }))
        .expect("an OpenAI function tool remains supported after normalization");
        assert_eq!(openai_client_tool["tools"][0]["name"], "lookup");
        assert!(openai_client_tool["tools"][0].get("type").is_none());
    }

    #[test]
    fn anthropic_request_allows_fast_mode_only_on_supported_models() {
        for model in ["claude-opus-5", "claude-opus-4-8"] {
            let fast = openai_to_anthropic_messages(json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "speed": "fast",
            }))
            .unwrap();
            assert_eq!(fast["speed"], "fast");
        }

        let error = openai_to_anthropic_messages(json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hello"}],
            "speed": "turbo",
        }))
        .unwrap_err();
        assert_eq!(error, "speed must be standard or fast");

        let standard = openai_to_anthropic_messages(json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hello"}],
            "speed": "standard",
        }))
        .unwrap();
        assert_eq!(standard["speed"], "standard");

        let unsupported = openai_to_anthropic_messages(json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hello"}],
            "speed": "fast",
        }))
        .unwrap_err();
        assert_eq!(
            unsupported,
            "speed=fast is unsupported for model claude-opus-4-7"
        );
    }

    #[test]
    fn anthropic_request_validates_inference_geo_before_admission() {
        for invalid in [json!("eu"), json!(1), Value::Null] {
            let request = json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "inference_geo": invalid
            });
            assert!(openai_to_anthropic_messages(request)
                .unwrap_err()
                .contains("inference_geo"));
        }

        for geo in ["global", "us"] {
            let converted = openai_to_anthropic_messages(json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "inference_geo": geo
            }))
            .unwrap();
            assert_eq!(converted["inference_geo"], geo);
        }

        let legacy = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [{"role": "user", "content": "hello"}],
            "inference_geo": "us"
        }))
        .unwrap_err();
        assert!(legacy.contains("unsupported for model claude-sonnet-4-5"));
    }

    #[test]
    fn anthropic_request_validates_cache_controls_before_admission() {
        for (cache_control, expected) in [
            (json!("ephemeral"), "cache_control must be an object"),
            (
                json!({"type": "persistent"}),
                "cache_control.type must be ephemeral",
            ),
            (
                json!({"type": "ephemeral", "ttl": "2h"}),
                "cache_control.ttl must be 5m or 1h",
            ),
        ] {
            let error = openai_to_anthropic_messages(json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "cache_control": cache_control
            }))
            .unwrap_err();
            assert_eq!(error, expected);
        }

        let nested = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-5",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "hello",
                    "cache_control": {"type": "ephemeral", "ttl": "forever"}
                }]
            }]
        }))
        .unwrap_err();
        assert_eq!(
            nested,
            "messages[0].content[0].cache_control.ttl must be 5m or 1h"
        );

        for cache_control in [
            json!({"type": "ephemeral"}),
            json!({"type": "ephemeral", "ttl": "5m"}),
            json!({"type": "ephemeral", "ttl": "1h"}),
        ] {
            assert!(openai_to_anthropic_messages(json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}],
                "cache_control": cache_control
            }))
            .is_ok());
        }
    }

    #[test]
    fn anthropic_request_requires_a_non_empty_model() {
        for model in [None, Some(""), Some("   ")] {
            let mut request = json!({
                "messages": [{"role": "user", "content": "hello"}]
            });
            if let Some(model) = model {
                request["model"] = json!(model);
            }
            assert_eq!(
                openai_to_anthropic_messages(request).unwrap_err(),
                "model must be a non-empty string"
            );
        }
    }

    #[test]
    fn anthropic_request_maps_assistant_tool_calls_and_tool_results() {
        let converted = openai_to_anthropic_messages(json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": "Compare Delhi and Mumbai weather"},
                {
                    "role": "assistant",
                    "content": "I will check both cities.",
                    "tool_calls": [
                        {
                            "id": "call_delhi",
                            "type": "function",
                            "function": {
                                "name": "lookup_weather",
                                "arguments": "{\"city\":\"Delhi\"}"
                            }
                        },
                        {
                            "id": "call_mumbai",
                            "type": "function",
                            "function": {
                                "name": "lookup_weather",
                                "arguments": "{\"city\":\"Mumbai\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_delhi",
                    "content": "31 C"
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_mumbai",
                    "content": "29 C"
                }
            ]
        }))
        .unwrap();

        assert_eq!(
            converted["messages"],
            json!([
                {"role": "user", "content": "Compare Delhi and Mumbai weather"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "I will check both cities."},
                        {
                            "type": "tool_use",
                            "id": "call_delhi",
                            "name": "lookup_weather",
                            "input": {"city": "Delhi"}
                        },
                        {
                            "type": "tool_use",
                            "id": "call_mumbai",
                            "name": "lookup_weather",
                            "input": {"city": "Mumbai"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_delhi",
                            "content": "31 C"
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_mumbai",
                            "content": "29 C"
                        }
                    ]
                }
            ])
        );
    }

    #[test]
    fn anthropic_request_rejects_non_adjacent_or_mismatched_tool_results() {
        let tool_call = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_weather",
                "type": "function",
                "function": {"name": "lookup_weather", "arguments": "{}"}
            }]
        });
        let cases = [
            (
                json!({
                    "messages": [{
                        "role": "tool",
                        "tool_call_id": "call_weather",
                        "content": "sunny"
                    }]
                }),
                "messages[0] is a tool result without a preceding assistant tool call",
            ),
            (
                json!({
                    "messages": [
                        tool_call.clone(),
                        {"role": "user", "content": "skip the tool"}
                    ]
                }),
                "messages[1] must return every pending tool result before the next message",
            ),
            (
                json!({
                    "messages": [
                        tool_call.clone(),
                        {"role": "tool", "tool_call_id": "call_other", "content": "sunny"}
                    ]
                }),
                "messages[1].tool_call_id does not match a pending assistant tool call: call_other",
            ),
            (
                json!({
                    "messages": [tool_call]
                }),
                "request ends before every pending tool result is returned",
            ),
        ];

        for (mut body, expected) in cases {
            body.as_object_mut()
                .expect("tool validation cases are objects")
                .insert("model".to_string(), json!("claude-sonnet-4-5"));
            assert_eq!(openai_to_anthropic_messages(body).unwrap_err(), expected);
        }
    }

    #[test]
    fn anthropic_request_rejects_invalid_shapes_with_stable_errors() {
        let cases = [
            (json!(null), "request body must be a JSON object"),
            (json!({}), "messages must be an array"),
            (json!({"messages": "hello"}), "messages must be an array"),
            (json!({"messages": [42]}), "messages[0] must be an object"),
            (
                json!({"messages": [{"role": "unknown", "content": "hello"}]}),
                "messages[0].role is unsupported: unknown",
            ),
            (
                json!({"messages": [{"role": "user", "content": "hello"}], "max_tokens": 0}),
                "max_tokens must be a positive integer",
            ),
            (
                json!({"messages": [{"role": "user", "content": "hello"}], "stop": ["END", 1]}),
                "stop must be a string or an array of strings",
            ),
            (
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "tools": [{"type": "function", "function": {"parameters": {"type": "object"}}}]
                }),
                "tools[0].function.name must be a non-empty string",
            ),
            (
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "tool_choice": {"type": "function", "function": {}}
                }),
                "tool_choice.function.name must be a non-empty string",
            ),
            (
                json!({
                    "messages": [{
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{"}
                        }]
                    }]
                }),
                "messages[0].tool_calls[0].function.arguments must encode a JSON object",
            ),
            (
                json!({"messages": [{"role": "tool", "content": "done"}]}),
                "messages[0].tool_call_id must be a non-empty string",
            ),
            (
                json!({"messages": [{"role": "user", "content": "hello"}], "n": 2}),
                "n must be 1 for Anthropic requests",
            ),
            (
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "parallel_tool_calls": "false"
                }),
                "parallel_tool_calls must be a boolean",
            ),
            (
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "functions": [{"name": "legacy"}]
                }),
                "legacy functions/function_call are unsupported; use tools/tool_choice",
            ),
            (
                json!({
                    "messages": [{
                        "role": "assistant",
                        "content": "hello",
                        "function_call": {"name": "legacy", "arguments": "{}"}
                    }]
                }),
                "messages[0].function_call is unsupported; use tool_calls",
            ),
            (
                json!({
                    "messages": [{
                        "role": "user",
                        "content": [{
                            "type": "image_url",
                            "image_url": {"url": "file:///tmp/private.png"}
                        }]
                    }]
                }),
                "messages[0].content[0].image_url.url must be an HTTP(S) URL or a supported base64 data URL",
            ),
            (
                json!({
                    "messages": [{
                        "role": "assistant",
                        "content": [{
                            "type": "image_url",
                            "image_url": {"url": "https://example.com/image.png"}
                        }]
                    }]
                }),
                "messages[0].content[0].type is unsupported for role assistant: image_url",
            ),
            (
                json!({
                    "messages": [{"role": "user", "content": "hello"}],
                    "temperature": -0.1
                }),
                "temperature must be non-negative",
            ),
            (
                json!({
                    "messages": [{
                        "role": "assistant",
                        "content": null,
                        "tool_calls": []
                    }]
                }),
                "messages[0].tool_calls must not be empty",
            ),
        ];

        for (mut body, expected) in cases {
            if body.get("messages").is_some_and(Value::is_array) {
                body.as_object_mut()
                    .expect("message validation cases are objects")
                    .insert("model".to_string(), json!("claude-sonnet-4-5"));
            }
            assert_eq!(
                openai_to_anthropic_messages(body).unwrap_err(),
                expected,
                "validation error changed"
            );
        }
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
        for alias in ["max_completion_tokens", "max_output_tokens"] {
            let mut aliased = json!({
                "model": "amazon.nova-micro-v1:0",
                "messages": [{"role": "user", "content": "hello"}]
            });
            aliased[alias] = json!(321);
            assert_eq!(
                openai_to_bedrock_converse(&aliased)["inferenceConfig"]["maxTokens"],
                321,
                "{alias}"
            );
        }
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
    fn response_converters_never_invent_missing_usage_halves() {
        for anthropic_usage in [Value::Null, json!({"input_tokens": 7})] {
            let mut response = json!({
                "id": "msg_partial",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-5",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn"
            });
            if !anthropic_usage.is_null() {
                response["usage"] = anthropic_usage;
            }
            let converted = transform_anthropic_to_openai_format(response, 1);
            assert_eq!(
                crate::policy::metering::extract_actual_usage(&converted),
                None
            );
        }

        for bedrock_usage in [Value::Null, json!({"inputTokens": 7})] {
            let mut response = json!({
                "output": {"message": {"content": [{"text": "ok"}]}},
                "stopReason": "end_turn"
            });
            if !bedrock_usage.is_null() {
                response["usage"] = bedrock_usage;
            }
            let converted = bedrock_converse_to_openai(&response, "amazon.nova-micro-v1:0", 1);
            assert_eq!(
                crate::policy::metering::extract_actual_usage(&converted),
                None
            );
        }
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
