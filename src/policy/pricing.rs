//! Model pricing table and cost estimation.
//!
//! Prices are USD per 1,000,000 tokens, standard synchronous tier, current as of
//! June 2026 (verified against official provider pricing pages). Documented
//! long-context tiers (OpenAI's >272K whole-request tier, Gemini's >200K tier)
//! ARE modeled — see [`LONG_CONTEXT_PRICING`]. Cached input, 1.25x cache
//! writes, batch rates, and per-request tool/search fees are NOT represented
//! here — callers that need exact cached/batch billing must adjust separately.
//! This table exists to support per-request cost annotation and the `cost_cap`
//! policy, not to be the system of record for billing.

/// `(model_id, input_usd_per_1m, output_usd_per_1m)`.
pub const MODEL_PRICING: &[(&str, f64, f64)] = &[
    // OpenAI — developers.openai.com/api/docs/models (GPT-5.6 family rates
    // confirmed against the official model pages 2026-08-04; the widely
    // reported 2026-07-30 "price cut" does not match what the model pages
    // bill, and overstating errs toward blocking early — the safe direction
    // for cost caps). The whole-request >272K tier for all three is in
    // `LONG_CONTEXT_PRICING`; the bare `gpt-5.6` alias is in `MODEL_ALIASES`.
    ("gpt-5.6-luna", 1.00, 6.00),
    ("gpt-5.6-terra", 2.50, 15.00),
    ("gpt-5.6-sol", 5.00, 30.00),
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
    // Anthropic — platform.claude.com/docs. Sonnet 5 is the INTRODUCTORY rate,
    // scheduled to rise to $3/$15 on 2026-09-01 — update this row then.
    ("claude-sonnet-5", 2.00, 10.00),
    ("claude-opus-4-8", 5.00, 25.00),
    ("claude-opus-4-7", 5.00, 25.00),
    ("claude-opus-4-6", 5.00, 25.00),
    ("claude-sonnet-4-6", 3.00, 15.00),
    ("claude-sonnet-4-5", 3.00, 15.00),
    ("claude-haiku-4-5", 1.00, 5.00),
    ("claude-fable-5", 10.00, 50.00),
    // Google Gemini — ai.google.dev/gemini-api/docs (3.6 Flash verified 2026-08;
    // Pro: <=200K-token tier here, the >200K tier is in `LONG_CONTEXT_PRICING`)
    ("gemini-3.6-flash", 1.50, 7.50),
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

/// Bare model aliases that providers resolve to a concrete model server-side,
/// `(alias, concrete_model_id)`. These MUST be resolved before family-prefix
/// matching: `gpt-5.6` would otherwise be read as a version-boundary extension
/// of the older, much cheaper `gpt-5` row, so predictive admission would
/// under-reserve an alias request before the provider response reveals which
/// concrete model actually ran.
const MODEL_ALIASES: &[(&str, &str)] = &[
    // developers.openai.com/api/docs/guides/latest-model — `gpt-5.6` routes to
    // the flagship Sol.
    ("gpt-5.6", "gpt-5.6-sol"),
];

/// Long-context pricing tiers: `(model_id, threshold_input_tokens,
/// high_input_usd_per_1m, high_output_usd_per_1m)`. When a request's input
/// exceeds the threshold, the whole request is estimated at the high-context
/// rate (matching how providers bill qualifying requests). Only models with a
/// documented tier are listed; others charge flat rates at any length.
const LONG_CONTEXT_PRICING: &[(&str, u32, f64, f64)] = &[
    // GPT-5.6 family: >272K input bills 2x input / 1.5x output for the whole
    // request. Documented identically on the Luna, Terra and Sol model pages,
    // so all three carry the tier (each derived from its own base row).
    ("gpt-5.6-luna", 272_000, 2.00, 9.00),
    ("gpt-5.6-terra", 272_000, 5.00, 22.50),
    ("gpt-5.6-sol", 272_000, 10.00, 45.00),
    // Gemini 2.5 Pro: >200K prompt tokens doubles both rates.
    ("gemini-2.5-pro", 200_000, 2.50, 15.00),
];

/// Normalize a caller-supplied model id: lowercase, then resolve a bare
/// provider alias (e.g. `gpt-5.6` → `gpt-5.6-sol`) to the concrete model it
/// routes to. Used by every lookup so alias requests price and tier exactly
/// like the model that will actually serve them.
fn canonical(model: &str) -> String {
    let m = model.to_lowercase();
    match MODEL_ALIASES.iter().find(|(alias, _)| *alias == m) {
        Some((_, target)) => (*target).to_string(),
        None => m,
    }
}

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
///
/// Bare provider aliases ([`MODEL_ALIASES`]) resolve to their concrete target
/// first, so `gpt-5.6` prices as Sol rather than as a snapshot of `gpt-5`.
pub fn lookup(model: &str) -> Option<ModelPrice> {
    let m = canonical(model);

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

/// Look up pricing for a model at a given input size, applying the model's
/// documented long-context tier when `input_tokens` exceeds its threshold.
pub fn lookup_for_context(model: &str, input_tokens: u32) -> Option<ModelPrice> {
    let base = lookup(model)?;
    let m = canonical(model);
    for &(id, threshold, hi_in, hi_out) in LONG_CONTEXT_PRICING {
        // Same matching rule as `lookup`: exact id or a dated snapshot of it.
        let is_family = m == id
            || (m.len() > id.len()
                && m.starts_with(id)
                && matches!(m.as_bytes()[id.len()], b'-' | b':' | b'.' | b'@' | b'/'));
        if is_family && input_tokens > threshold {
            return Some(ModelPrice {
                input_per_1m: hi_in,
                output_per_1m: hi_out,
            });
        }
    }
    Some(base)
}

/// Estimate the USD cost of a call given token counts. Applies long-context
/// tier rates when the input size qualifies. Unknown models cost 0.0 (the
/// caller decides whether unknown-model cost should fail open or closed).
pub fn estimate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    match lookup_for_context(model, input_tokens) {
        Some(p) => {
            (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
                + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m
        }
        None => 0.0,
    }
}

/// Assumed completion size when the request doesn't set `max_tokens`: cost caps
/// need *some* forward estimate of the call being admitted, and most chat
/// completions finish well under this. Erring high only blocks slightly before
/// the cap instead of after it — the right direction for a hard cap. (An
/// unbounded request can exceed this — models allow up to 128K output — but
/// reserving a model's full output ceiling for every unbounded chat request
/// would block ordinary traffic whenever cap headroom drops below ~$1;
/// operators who want stricter admission raise the assumption via
/// `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS`.)
pub const DEFAULT_ASSUMED_OUTPUT_TOKENS: u64 = 1024;

/// The assumed completion size for requests without an explicit output limit:
/// `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` when set, else
/// [`DEFAULT_ASSUMED_OUTPUT_TOKENS`]. Read once (it feeds every admission).
pub fn assumed_output_tokens() -> u64 {
    static ASSUMED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ASSUMED.get_or_init(|| {
        std::env::var("NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_ASSUMED_OUTPUT_TOKENS)
    })
}

/// Predict the cost of a request *before* forwarding it: estimated input tokens
/// plus the request's `max_tokens` (or the configured assumed completion size)
/// at the model's applicable (context-dependent) rates. `None` when the model
/// has no pricing entry — the caller decides whether an unmeterable call fails
/// open or closed.
pub fn estimate_request_cost(
    model: &str,
    input_tokens: u32,
    max_output_tokens: Option<u64>,
) -> Option<f64> {
    let p = lookup_for_context(model, input_tokens)?;
    let out = max_output_tokens.unwrap_or_else(assumed_output_tokens);
    Some(
        (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
            + (out as f64 / 1_000_000.0) * p.output_per_1m,
    )
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

    #[test]
    fn current_generation_models_priced() {
        // Current-generation ids must resolve to their own (verified) rates,
        // not fall back to an older family or to $0.
        let luna = lookup("gpt-5.6-luna").unwrap();
        assert_eq!((luna.input_per_1m, luna.output_per_1m), (1.00, 6.00));
        // Terra/Sol must have their own rows — falling through to the shorter
        // "gpt-5" family prefix would undercount them (Sol by 4x).
        let terra = lookup("gpt-5.6-terra").unwrap();
        assert_eq!((terra.input_per_1m, terra.output_per_1m), (2.50, 15.00));
        let sol = lookup("gpt-5.6-sol").unwrap();
        assert_eq!((sol.input_per_1m, sol.output_per_1m), (5.00, 30.00));
        let sonnet5 = lookup("claude-sonnet-5").unwrap();
        assert_eq!((sonnet5.input_per_1m, sonnet5.output_per_1m), (2.00, 10.00));
        let flash = lookup("gemini-3.6-flash").unwrap();
        assert_eq!((flash.input_per_1m, flash.output_per_1m), (1.50, 7.50));
        // Dated snapshots resolve to the same family.
        assert_eq!(
            lookup("gpt-5.6-luna-2026-05-01").unwrap().input_per_1m,
            1.00
        );
        assert_eq!(
            lookup("claude-sonnet-5-20260601").unwrap().input_per_1m,
            2.00
        );
    }

    #[test]
    fn bare_gpt_5_6_alias_resolves_to_sol() {
        // The provider resolves `gpt-5.6` to `gpt-5.6-sol`. Without an alias
        // row the `.` reads as a version boundary and the id degrades to the
        // much cheaper `gpt-5` family, under-reserving predictive admission.
        let alias = lookup("gpt-5.6").unwrap();
        assert_eq!((alias.input_per_1m, alias.output_per_1m), (5.00, 30.00));
        assert_eq!(alias, lookup("gpt-5.6-sol").unwrap());
        assert_ne!(alias, lookup("gpt-5").unwrap());
        // Case-insensitive, like every other lookup.
        assert_eq!(lookup("GPT-5.6").unwrap(), alias);
        // Predictive admission reserves Sol rates for the alias: 6 in / 8 out.
        let r = estimate_request_cost("gpt-5.6", 6, Some(8)).unwrap();
        let expected = (6.0 / 1e6) * 5.00 + (8.0 / 1e6) * 30.00;
        assert!(
            (r - expected).abs() < 1e-12,
            "alias reserved {r} vs {expected}"
        );
        // ...which is far more than the stale `gpt-5` fallback would reserve.
        let stale = estimate_request_cost("gpt-5", 6, Some(8)).unwrap();
        assert!(r > stale * 2.0);
        // The alias inherits Sol's long-context tier as well.
        let hi = lookup_for_context("gpt-5.6", 300_000).unwrap();
        assert_eq!((hi.input_per_1m, hi.output_per_1m), (10.00, 45.00));
    }

    #[test]
    fn long_context_tier_applies_to_whole_gpt_5_6_family() {
        // OpenAI documents the same >272K whole-request tier (2x in / 1.5x out)
        // on the Luna, Terra and Sol pages — all three must carry it.
        for (model, std_rates, hi_rates) in [
            ("gpt-5.6-luna", (1.00, 6.00), (2.00, 9.00)),
            ("gpt-5.6-terra", (2.50, 15.00), (5.00, 22.50)),
            ("gpt-5.6-sol", (5.00, 30.00), (10.00, 45.00)),
        ] {
            // At the boundary: standard rates.
            let at = lookup_for_context(model, 272_000).unwrap();
            assert_eq!(
                (at.input_per_1m, at.output_per_1m),
                std_rates,
                "{model} @272K"
            );
            // One token above it: the high-context rates.
            let over = lookup_for_context(model, 272_001).unwrap();
            assert_eq!(
                (over.input_per_1m, over.output_per_1m),
                hi_rates,
                "{model} >272K"
            );
            // The tier multipliers are exactly 2x input / 1.5x output.
            assert!((hi_rates.0 - std_rates.0 * 2.0).abs() < 1e-9);
            assert!((hi_rates.1 - std_rates.1 * 1.5).abs() < 1e-9);
            // Predictive admission at 300K in / 1K out uses the tier, not the
            // standard rate (which would under-reserve by ~50%).
            let reserved = estimate_request_cost(model, 300_000, Some(1_000)).unwrap();
            let expected = (300_000.0 / 1e6) * hi_rates.0 + (1_000.0 / 1e6) * hi_rates.1;
            assert!(
                (reserved - expected).abs() < 1e-9,
                "{model} reserved {reserved}"
            );
            let flat = (300_000.0 / 1e6) * std_rates.0 + (1_000.0 / 1e6) * std_rates.1;
            assert!(
                reserved > flat * 1.9,
                "{model} tier must roughly double the flat estimate"
            );
        }
    }

    #[test]
    fn long_context_tier_applies_above_threshold() {
        // Below/at the threshold: standard rates.
        let base = lookup_for_context("gpt-5.6-luna", 272_000).unwrap();
        assert_eq!((base.input_per_1m, base.output_per_1m), (1.00, 6.00));
        // Above it: the documented high-context rates (2x in / 1.5x out).
        let hi = lookup_for_context("gpt-5.6-luna", 272_001).unwrap();
        assert_eq!((hi.input_per_1m, hi.output_per_1m), (2.00, 9.00));
        // Dated snapshots of a tiered family get the tier too.
        let hi_snap = lookup_for_context("gpt-5.6-luna-2026-05-01", 300_000).unwrap();
        assert_eq!(hi_snap.input_per_1m, 2.00);
        // Gemini 2.5 Pro doubles above 200K prompt tokens.
        let g = lookup_for_context("gemini-2.5-pro", 250_000).unwrap();
        assert_eq!((g.input_per_1m, g.output_per_1m), (2.50, 15.00));
        // Models without a documented tier keep flat rates at any size.
        let flat = lookup_for_context("gpt-4o", 5_000_000).unwrap();
        assert_eq!(flat.input_per_1m, 2.50);
        // estimate_cost and estimate_request_cost pick the tier as well.
        let c = estimate_cost("gpt-5.6-luna", 300_000, 1000);
        let expected = (300_000.0 / 1e6) * 2.00 + (1000.0 / 1e6) * 9.00;
        assert!((c - expected).abs() < 1e-9);
        let r = estimate_request_cost("gpt-5.6-luna", 300_000, Some(1000)).unwrap();
        assert!((r - expected).abs() < 1e-9);
    }

    #[test]
    fn estimate_request_cost_predicts_with_and_without_max_tokens() {
        // gpt-4o: 10 input @ 2.50/1M + 1000 max output @ 10.00/1M
        let c = estimate_request_cost("gpt-4o", 10, Some(1000)).unwrap();
        let expected = (10.0 / 1e6) * 2.50 + (1000.0 / 1e6) * 10.00;
        assert!((c - expected).abs() < 1e-12);
        // Without max_tokens the default assumed completion applies.
        let d = estimate_request_cost("gpt-4o", 10, None).unwrap();
        let expected_default =
            (10.0 / 1e6) * 2.50 + (DEFAULT_ASSUMED_OUTPUT_TOKENS as f64 / 1e6) * 10.00;
        assert!((d - expected_default).abs() < 1e-12);
        // Unpriced model → None (caller decides open/closed).
        assert!(estimate_request_cost("no-such-model", 10, Some(10)).is_none());
    }
}
