//! Model pricing table and cost estimation.
//!
//! Prices are USD per 1,000,000 tokens, standard synchronous tier, current as of
//! June 2026 (verified against official provider pricing pages). Cached-input,
//! batch, and Gemini's >200K tier are NOT represented here — callers that need
//! exact cached/batch billing must adjust separately. This table exists to
//! support per-request cost annotation and the `cost_cap` policy, not to be the
//! system of record for billing.

/// `(model_id, input_usd_per_1m, output_usd_per_1m)`.
pub const MODEL_PRICING: &[(&str, f64, f64)] = &[
    // OpenAI — platform.openai.com/docs/pricing
    ("gpt-5", 1.25, 10.00),
    ("gpt-5-mini", 0.25, 2.00),
    ("gpt-5-nano", 0.05, 0.40),
    ("gpt-4.1", 2.00, 8.00),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1-nano", 0.10, 0.40),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4o-mini", 0.15, 0.60),
    ("o3", 2.00, 8.00),
    ("o4-mini", 1.10, 4.40),
    ("o1", 15.00, 60.00),
    ("text-embedding-3-small", 0.02, 0.00),
    ("text-embedding-3-large", 0.13, 0.00),
    // Anthropic — docs.claude.com/pricing
    ("claude-opus-4-8", 5.00, 25.00),
    ("claude-opus-4-7", 5.00, 25.00),
    ("claude-opus-4-6", 5.00, 25.00),
    ("claude-sonnet-4-6", 3.00, 15.00),
    ("claude-sonnet-4-5", 3.00, 15.00),
    ("claude-haiku-4-5", 1.00, 5.00),
    ("claude-fable-5", 10.00, 50.00),
    // Google Gemini — ai.google.dev/pricing (Pro: <=200K-token tier; doubles above)
    ("gemini-2.5-pro", 1.25, 10.00),
    ("gemini-2.5-flash", 0.30, 2.50),
    ("gemini-2.5-flash-lite", 0.10, 0.40),
    // Groq — groq.com/pricing
    ("llama-3.3-70b-versatile", 0.59, 0.79),
    ("llama-3.1-8b-instant", 0.05, 0.08),
    ("meta-llama/llama-4-scout-17b-16e-instruct", 0.11, 0.34),
    ("openai/gpt-oss-120b", 0.15, 0.60),
    ("openai/gpt-oss-20b", 0.075, 0.30),
    // Mistral — mistral.ai/pricing
    ("mistral-large-latest", 0.50, 1.50),
    ("mistral-medium-latest", 1.50, 7.50),
    ("mistral-small-latest", 0.15, 0.60),
    ("codestral-latest", 0.30, 0.90),
    ("magistral-medium-latest", 2.00, 5.00),
    // Cohere — cohere.com/pricing
    ("command-a-03-2025", 2.50, 10.00),
    ("command-r-plus-08-2024", 2.50, 10.00),
    ("command-r-08-2024", 0.15, 0.60),
    ("command-r7b-12-2024", 0.0375, 0.15),
    // Together AI — together.ai/pricing
    ("meta-llama/llama-3.3-70b-instruct-turbo", 1.04, 1.04),
    (
        "meta-llama/llama-4-maverick-17b-128e-instruct-fp8",
        0.27,
        0.85,
    ),
    ("meta-llama/llama-4-scout-17b-16e-instruct", 0.18, 0.59),
    ("deepseek-ai/deepseek-v3", 1.25, 1.25),
    // Fireworks AI — fireworks.ai/pricing
    ("accounts/fireworks/models/deepseek-v4-pro", 1.74, 3.48),
    ("accounts/fireworks/models/deepseek-v4-flash", 0.14, 0.28),
    ("accounts/fireworks/models/kimi-k2p6", 0.95, 4.00),
    (
        "accounts/fireworks/models/llama-v3p3-70b-instruct",
        0.90,
        0.90,
    ),
    // AWS Bedrock — aws.amazon.com/bedrock/pricing (US on-demand)
    ("anthropic.claude-opus-4-5-20251101-v1:0", 5.00, 25.00),
    ("anthropic.claude-sonnet-4-5-20250929-v1:0", 3.00, 15.00),
    ("anthropic.claude-haiku-4-5-20251001-v1:0", 1.00, 5.00),
    ("amazon.nova-pro-v1:0", 0.80, 3.20),
    ("amazon.nova-lite-v1:0", 0.06, 0.24),
    ("amazon.nova-micro-v1:0", 0.035, 0.14),
    ("amazon.nova-premier-v1:0", 2.50, 12.50),
    // DeepSeek — api-docs.deepseek.com (cache-miss input)
    ("deepseek-v4-flash", 0.14, 0.28),
    ("deepseek-v4-pro", 0.435, 0.87),
    ("deepseek-chat", 0.27, 1.10),
    ("deepseek-reasoner", 0.55, 2.19),
    // xAI — x.ai/api
    ("grok-4.3", 1.25, 2.50),
    ("grok-build-0.1", 1.00, 2.00),
    // Perplexity — docs.perplexity.ai (token cost only; + per-request search fees)
    ("sonar", 1.00, 1.00),
    ("sonar-pro", 3.00, 15.00),
    ("sonar-reasoning-pro", 2.00, 8.00),
];

/// Pricing for one model: USD per 1M input/output tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

/// Look up pricing for a model id.
///
/// Matching is case-insensitive: an exact match wins; otherwise the input may be
/// a *dated snapshot* of a known family (e.g. `gpt-4o-2024-11-20` → `gpt-4o`), in
/// which case the longest table id that is a prefix of the input wins, but only
/// when the next character is a version separator (`-`, `:`, `.`, `@`) so that
/// `gpt-4` cannot match `gpt-4o`. A short or garbage id that is merely a prefix
/// of a table entry returns `None` (never the other direction). Returns `None`
/// when no family matches.
pub fn lookup(model: &str) -> Option<ModelPrice> {
    let m = model.to_lowercase();

    // Exact match first.
    if let Some(&(_, i, o)) = MODEL_PRICING.iter().find(|(id, _, _)| *id == m) {
        return Some(ModelPrice {
            input_per_1m: i,
            output_per_1m: o,
        });
    }

    // Longest known family that the input *extends* at a version boundary.
    let mut best: Option<(usize, f64, f64)> = None;
    for &(id, i, o) in MODEL_PRICING {
        if m.len() > id.len() && m.starts_with(id) {
            let boundary = m.as_bytes()[id.len()];
            if matches!(boundary, b'-' | b':' | b'.' | b'@' | b'/')
                && best.is_none_or(|(blen, _, _)| id.len() > blen)
            {
                best = Some((id.len(), i, o));
            }
        }
    }
    best.map(|(_, i, o)| ModelPrice {
        input_per_1m: i,
        output_per_1m: o,
    })
}

/// Estimate the USD cost of a call given token counts. Unknown models cost 0.0
/// (the caller decides whether unknown-model cost should fail open or closed).
pub fn estimate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    match lookup(model) {
        Some(p) => {
            (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
                + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m
        }
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lookup() {
        let p = lookup("gpt-4o").unwrap();
        assert_eq!(p.input_per_1m, 2.50);
        assert_eq!(p.output_per_1m, 10.00);
    }

    #[test]
    fn case_insensitive_lookup() {
        assert_eq!(lookup("GPT-4O"), lookup("gpt-4o"));
        assert_eq!(lookup("Claude-Opus-4-8"), lookup("claude-opus-4-8"));
    }

    #[test]
    fn dated_snapshot_family_match() {
        // a dated snapshot should fall back to the family price
        let p = lookup("gpt-4o-2024-11-20");
        assert!(p.is_some());
        assert_eq!(p.unwrap().input_per_1m, 2.50);
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(lookup("totally-made-up-model-xyz").is_none());
    }

    #[test]
    fn short_or_garbage_ids_do_not_mismatch() {
        // Prefix-of-a-table-entry must NOT resolve (the bug: "g" -> gpt-5).
        assert!(lookup("g").is_none());
        assert!(lookup("gpt").is_none());
        assert!(lookup("gpt-").is_none());
        // "gpt-4" is shorter than "gpt-4o" and not an exact entry -> None.
        assert!(lookup("gpt-4").is_none());
        assert!(lookup("claude").is_none());
        assert!(lookup("o").is_none());
    }

    #[test]
    fn family_match_requires_version_boundary() {
        // "gpt-4ox" is not a dated snapshot of gpt-4o (no separator) -> None.
        assert!(lookup("gpt-4oxyz").is_none());
        // but a real separator resolves to the family
        assert_eq!(lookup("gpt-4o:free").unwrap().input_per_1m, 2.50);
        assert_eq!(lookup("gpt-4o-mini-2024-07-18").unwrap().input_per_1m, 0.15);
    }

    #[test]
    fn estimate_cost_math() {
        // gpt-4o: 1M input @ 2.50, 1M output @ 10.00
        let c = estimate_cost("gpt-4o", 1_000_000, 1_000_000);
        assert!((c - 12.50).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_partial_tokens() {
        // 1000 input + 500 output on gpt-4o-mini (0.15 / 0.60 per 1M)
        let c = estimate_cost("gpt-4o-mini", 1000, 500);
        let expected = (1000.0 / 1e6) * 0.15 + (500.0 / 1e6) * 0.60;
        assert!((c - expected).abs() < 1e-12);
    }

    #[test]
    fn unknown_model_cost_zero() {
        assert_eq!(estimate_cost("nope", 1000, 1000), 0.0);
    }

    #[test]
    fn anthropic_and_bedrock_priced() {
        assert!(lookup("claude-opus-4-8").is_some());
        assert!(lookup("anthropic.claude-haiku-4-5-20251001-v1:0").is_some());
    }
}
