//! Model pricing table and cost estimation.
//!
//! Prices are USD per 1,000,000 tokens, standard synchronous tier, verified
//! against official provider pricing pages on 2026-08-09. Documented
//! long-context tiers (OpenAI's >272K whole-request tier, Gemini's >200K tier)
//! ARE modeled — see [`LONG_CONTEXT_PRICING`] — and announced future price
//! changes are modeled in [`SCHEDULED_PRICING`] so they take effect on their
//! own date instead of requiring a manual edit. Cached input, 1.25x cache
//! writes, batch rates, and per-request tool/search fees are NOT represented
//! here — callers that need exact cached/batch billing must adjust separately.
//! This table exists to support per-request cost annotation and the `cost_cap`
//! policy, not to be the system of record for billing.

use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

/// `(model_id, input_usd_per_1m, output_usd_per_1m)`.
pub const MODEL_PRICING: &[(&str, f64, f64)] = &[
    // OpenAI — developers.openai.com/api/docs/models. GPT-5.6 family rates read
    // off the official Luna/Terra/Sol model pages on 2026-08-09. The
    // whole-request >272K tier for all three is in `LONG_CONTEXT_PRICING`; the
    // bare `gpt-5.6` alias is in `MODEL_ALIASES`.
    ("gpt-5.6-luna", 0.20, 1.20),
    ("gpt-5.6-terra", 2.00, 12.00),
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
    // in force through 2026-08-31; the announced $3/$15 standard rate takes
    // over automatically on 2026-09-01 via `SCHEDULED_PRICING`.
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
    ("gpt-5.6-luna", 272_000, 0.40, 1.80),
    ("gpt-5.6-terra", 272_000, 4.00, 18.00),
    ("gpt-5.6-sol", 272_000, 10.00, 45.00),
    // Gemini 2.5 Pro: >200K prompt tokens doubles both rates.
    ("gemini-2.5-pro", 200_000, 2.50, 15.00),
];

/// `2026-09-01T00:00:00Z` — when Claude Sonnet 5's introductory rate ends.
/// Asserted against the parsed RFC 3339 literal in the tests below so the
/// magic number can't silently drift.
const SONNET_5_STANDARD_FROM: i64 = 1_788_220_800;

/// Announced future price changes: `(model_id, effective_from_unix_secs,
/// input_usd_per_1m, output_usd_per_1m)`. The latest entry whose
/// `effective_from` has passed replaces the model's [`MODEL_PRICING`] base row.
///
/// This exists so a *dated, already-published* price change lands on its own
/// date instead of needing an emergency edit that morning. It applies to base
/// rates only — [`lookup_at`] still reads long-context tiers from
/// [`LONG_CONTEXT_PRICING`], and `scheduled_models_have_no_long_context_tier`
/// fails the build's test suite if a scheduled model ever gains a tier row,
/// which is when this would need to become date-aware too.
const SCHEDULED_PRICING: &[(&str, i64, f64, f64)] = &[
    // platform.claude.com/docs/en/release-notes/overview — "introductory
    // pricing of $2 / $10 per MTok through August 31, 2026 (standard $3 / $15
    // thereafter)".
    ("claude-sonnet-5", SONNET_5_STANDARD_FROM, 3.00, 15.00),
];

/// Current wall-clock time as unix seconds, on both build targets.
/// `chrono::Utc::now()` is unavailable under wasm32, where the Worker runtime
/// supplies the clock instead.
fn now_unix_seconds() -> i64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Utc::now().timestamp()
    }
    #[cfg(target_arch = "wasm32")]
    {
        (worker::Date::now().as_millis() / 1000) as i64
    }
}

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

/// How a call was priced. The unknown-model case is part of the type so it
/// cannot be mistaken for "priced, and it happened to be free": a model that
/// matches nothing in the catalog used to fall through to $0, which made its
/// spend invisible to cost caps and to the usage reports the platform bills
/// from. Every priced path now yields one of these two variants, and both
/// carry a non-zero rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelPricing {
    /// Rates published for this model (or the family/alias it resolves to).
    Catalog(ModelPrice),
    /// Nothing in the catalog matched. Priced at [`assumed_unknown_price`] so
    /// the call still advances cost counters; the rate is an assumption, not a
    /// quote, so anything that reconciles against a provider invoice must treat
    /// it as an upper bound rather than as truth.
    UnknownAssumed(ModelPrice),
}

impl ModelPricing {
    /// The rates to bill at, whichever variant this is.
    pub fn price(self) -> ModelPrice {
        match self {
            ModelPricing::Catalog(p) | ModelPricing::UnknownAssumed(p) => p,
        }
    }

    /// The rates, but only when they came from the catalog — for callers that
    /// must distinguish "we know what this costs" from "we guessed high".
    pub fn catalog(self) -> Option<ModelPrice> {
        match self {
            ModelPricing::Catalog(p) => Some(p),
            ModelPricing::UnknownAssumed(_) => None,
        }
    }

    /// Whether these rates are the defensive assumption for an unknown model.
    pub fn is_assumed(self) -> bool {
        matches!(self, ModelPricing::UnknownAssumed(_))
    }
}

/// A priced call: the amount, plus the basis it was priced on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub usd: f64,
    pub pricing: ModelPricing,
}

impl CostEstimate {
    /// Whether this amount rests on the unknown-model assumption.
    pub fn is_assumed(self) -> bool {
        self.pricing.is_assumed()
    }
}

/// USD per 1M tokens charged to a model that matches nothing in the catalog.
///
/// **Derived from the catalog rather than hard-coded**: the maximum input rate
/// and the maximum output rate over every published row — base
/// ([`MODEL_PRICING`]), announced ([`SCHEDULED_PRICING`]) and long-context
/// ([`LONG_CONTEXT_PRICING`]). Today that is `o1`'s $15.00 / $60.00 per 1M.
/// Adding a pricier model automatically raises the assumption, so an unknown
/// model can never be estimated below the most expensive call the gateway
/// knows how to make (`assumed_rate_bounds_every_catalog_rate` pins this).
///
/// Erring high is the intended direction. An unknown id is either a newly
/// released model — historically priced at or near the top of the market — or
/// a typo. Over-estimating a typo costs a request that gets blocked slightly
/// early and an alertable warning; under-estimating a real model leaks spend
/// past every cost cap silently, which is exactly the bug this replaces.
///
/// NOTE: the *number* is a defensive default, not a ratified pricing policy
/// (see NOV-135). Whether an unknown model should instead be rejected outright
/// is likewise a policy decision; this function only guarantees it is never $0.
pub fn assumed_unknown_price() -> ModelPrice {
    static ASSUMED: OnceLock<ModelPrice> = OnceLock::new();
    *ASSUMED.get_or_init(|| {
        let rows = MODEL_PRICING
            .iter()
            .map(|&(_, i, o)| (i, o))
            .chain(SCHEDULED_PRICING.iter().map(|&(_, _, i, o)| (i, o)))
            .chain(LONG_CONTEXT_PRICING.iter().map(|&(_, _, i, o)| (i, o)));
        let (input_per_1m, output_per_1m) = rows.fold((0.0_f64, 0.0_f64), |(mi, mo), (i, o)| {
            (mi.max(i), mo.max(o))
        });
        ModelPrice {
            input_per_1m,
            output_per_1m,
        }
    })
}

/// Distinct unknown model ids to remember before resetting the warn-dedup set.
/// Bounded because model ids come from untrusted request bodies: an unbounded
/// set would grow without limit, and warning on *every* lookup would let one
/// hot unknown model flood the log. On overflow the set clears, so a persistent
/// unknown model re-warns periodically instead of going quiet forever.
const UNKNOWN_MODEL_WARN_CAPACITY: usize = 256;

/// Emit one alertable event per distinct unknown model id.
fn warn_unknown_model(model: &str, canonical_id: &str, assumed: ModelPrice) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.len() >= UNKNOWN_MODEL_WARN_CAPACITY {
        seen.clear();
    }
    if !seen.insert(canonical_id.to_string()) {
        return;
    }
    drop(seen);
    warn!(
        model = %model,
        canonical_model = %canonical_id,
        assumed_input_usd_per_1m = assumed.input_per_1m,
        assumed_output_usd_per_1m = assumed.output_per_1m,
        "Nova Guard: no pricing entry for model; billing at the assumed maximum catalog rate"
    );
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
    lookup_at(model, 0, now())
}

/// Whether canonical id `m` is `id` itself or a dated snapshot that extends it
/// at a version boundary (`gpt-4o-2024-11-20` is a snapshot of `gpt-4o`, but
/// `gpt-4oxyz` is not a snapshot of `gpt-4o`). The one matching rule shared by
/// the base table and the long-context tier table, so the two cannot drift.
fn is_family(m: &str, id: &str) -> bool {
    m == id
        || (m.len() > id.len()
            && m.starts_with(id)
            && matches!(m.as_bytes()[id.len()], b'-' | b':' | b'.' | b'@' | b'/'))
}

/// The [`MODEL_PRICING`] row a canonical id resolves to: exact match wins,
/// otherwise the longest family the id extends at a version boundary. Returns
/// the matched *table id* alongside its rates so scheduled overrides and
/// long-context tiers can be keyed off the same row a dated snapshot resolved
/// to.
fn match_row(m: &str) -> Option<(&'static str, f64, f64)> {
    if let Some(&(id, i, o)) = MODEL_PRICING.iter().find(|(id, _, _)| *id == m) {
        return Some((id, i, o));
    }
    let mut best: Option<(&'static str, f64, f64)> = None;
    for &(id, i, o) in MODEL_PRICING {
        if m.len() > id.len()
            && m.starts_with(id)
            && matches!(m.as_bytes()[id.len()], b'-' | b':' | b'.' | b'@' | b'/')
            && best.is_none_or(|(bid, _, _)| id.len() > bid.len())
        {
            best = Some((id, i, o));
        }
    }
    best
}

/// Now, as the type [`lookup_at`] takes. Production lookups use this; tests
/// inject fixed instants instead so a dated price change is verifiable on both
/// sides of its boundary without waiting for the calendar.
fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(now_unix_seconds(), 0).unwrap_or(DateTime::UNIX_EPOCH)
}

/// Look up pricing for a model at a given input size, **as of a given
/// instant** — applying any [`SCHEDULED_PRICING`] change that has taken effect
/// by `at`, then the model's documented long-context tier when `input_tokens`
/// exceeds its threshold.
pub fn lookup_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<ModelPrice> {
    resolve_at(model, input_tokens, at)
}

/// Price a model at a given input size and instant, **always** yielding rates:
/// the catalog's when the id resolves, otherwise [`assumed_unknown_price`] —
/// tagged as an assumption in the return type. Unlike [`lookup_at`] this never
/// returns `None`, so no caller can accidentally meter an unpriced call at $0.
///
/// Emits one alertable `warn` per distinct unknown model id.
pub fn price_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> ModelPricing {
    match resolve_at(model, input_tokens, at) {
        Some(p) => ModelPricing::Catalog(p),
        None => {
            let assumed = assumed_unknown_price();
            warn_unknown_model(model, &canonical(model), assumed);
            ModelPricing::UnknownAssumed(assumed)
        }
    }
}

/// Price a model at a given input size, at the current time. See [`price_at`].
pub fn price_for_context(model: &str, input_tokens: u32) -> ModelPricing {
    price_at(model, input_tokens, now())
}

/// The catalog rates for a model, or `None` when nothing matches. The shared
/// body of [`lookup_at`] (advisory: "do we know this model?") and [`price_at`]
/// (billing: "what do we charge?"), so the two can never disagree about which
/// ids are known. Deliberately silent — [`price_at`] owns the warning, so
/// advisory lookups don't double-log every priced request.
fn resolve_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<ModelPrice> {
    let m = canonical(model);
    let (row_id, base_in, base_out) = match_row(&m)?;

    // A long-context tier prices the whole request and supersedes the base row.
    for &(id, threshold, hi_in, hi_out) in LONG_CONTEXT_PRICING {
        if is_family(&m, id) && input_tokens > threshold {
            return Some(ModelPrice {
                input_per_1m: hi_in,
                output_per_1m: hi_out,
            });
        }
    }

    // Otherwise the base row, unless a scheduled change has taken effect. The
    // latest effective entry wins, so a model may carry several dated steps.
    let mut price = ModelPrice {
        input_per_1m: base_in,
        output_per_1m: base_out,
    };
    let mut effective_from = i64::MIN;
    for &(id, from, sched_in, sched_out) in SCHEDULED_PRICING {
        if id == row_id && at.timestamp() >= from && from > effective_from {
            effective_from = from;
            price = ModelPrice {
                input_per_1m: sched_in,
                output_per_1m: sched_out,
            };
        }
    }
    Some(price)
}

/// Look up pricing for a model at a given input size, applying the model's
/// documented long-context tier when `input_tokens` exceeds its threshold, at
/// the current time. See [`lookup_at`] to price as of a specific instant.
pub fn lookup_for_context(model: &str, input_tokens: u32) -> Option<ModelPrice> {
    lookup_at(model, input_tokens, now())
}

/// Price a completed call, reporting both the amount and the basis it rests on.
/// Applies long-context tier rates when the input size qualifies. A model with
/// no catalog entry is priced at [`assumed_unknown_price`] and tagged
/// [`ModelPricing::UnknownAssumed`] — never $0, so an unmetered call can't be
/// recorded as a free one.
pub fn price_call(model: &str, input_tokens: u32, output_tokens: u32) -> CostEstimate {
    let pricing = price_for_context(model, input_tokens);
    let p = pricing.price();
    CostEstimate {
        usd: (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
            + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m,
        pricing,
    }
}

/// Estimate the USD cost of a call **from the catalog only**, returning `0.0`
/// for a model with no entry.
///
/// This is the provider extractors' entry point, and they read `0.0` as
/// "unpriced" (recording `cost: None` rather than a fake zero). The unknown
/// model is not left free: `telemetry::middleware` backfills [`price_call`]'s
/// defensive estimate at the metering boundary, before the usage event that
/// cost caps and billing read. Prefer [`price_call`] in new code — it makes
/// the unknown case impossible to overlook.
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
/// open or closed (`cost_cap` blocks unknown models when `fail_closed`).
///
/// This is the *admission* path and still fails open by returning `None`;
/// switching it to reserve [`assumed_unknown_price`] instead — i.e. letting an
/// unknown model consume cap headroom before it runs — is a policy decision
/// pending NOV-135. The *billing* path ([`price_call`]) is already defensive.
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
    fn unknown_model_is_never_free() {
        // The bug: a model absent from the catalog priced at $0, so its spend
        // was invisible to cost caps and to the usage reports we bill from.
        let a = assumed_unknown_price();
        let e = price_call("nope", 1000, 1000);
        assert!(e.is_assumed(), "unknown model must be tagged as assumed");
        assert_eq!(e.pricing, ModelPricing::UnknownAssumed(a));
        let expected = (1000.0 / 1e6) * a.input_per_1m + (1000.0 / 1e6) * a.output_per_1m;
        assert!((e.usd - expected).abs() < 1e-12);
        assert!(e.usd > 0.0, "unknown model must never cost $0");
        // Zero tokens is the only way to reach $0, and it is honestly free.
        assert_eq!(price_call("nope", 0, 0).usd, 0.0);
        // A known model is unaffected by the defensive path.
        assert_eq!(
            price_call("gpt-4o", 1000, 500).usd,
            estimate_cost("gpt-4o", 1000, 500)
        );
        // `estimate_cost` stays catalog-only (0.0 = unpriced) for the provider
        // extractors; the middleware backfills `price_call` before the usage
        // event, which is where an unknown model stops being free.
        assert_eq!(estimate_cost("nope", 1000, 1000), 0.0);
    }

    #[test]
    fn assumed_rate_bounds_every_catalog_rate() {
        // The assumption is derived from the catalog, so it can't go stale when
        // a pricier model lands: no published rate may exceed it.
        let a = assumed_unknown_price();
        for &(id, i, o) in MODEL_PRICING {
            assert!(i <= a.input_per_1m, "{id} input {i} > assumed");
            assert!(o <= a.output_per_1m, "{id} output {o} > assumed");
        }
        for &(id, _, i, o) in SCHEDULED_PRICING {
            assert!(i <= a.input_per_1m, "{id} scheduled input {i} > assumed");
            assert!(o <= a.output_per_1m, "{id} scheduled output {o} > assumed");
        }
        for &(id, _, i, o) in LONG_CONTEXT_PRICING {
            assert!(i <= a.input_per_1m, "{id} tier input {i} > assumed");
            assert!(o <= a.output_per_1m, "{id} tier output {o} > assumed");
        }
        // Today the ceiling is o1 ($15 / $60 per 1M).
        assert_eq!((a.input_per_1m, a.output_per_1m), (15.00, 60.00));
        // An unknown model must cost at least as much as any known one.
        let unknown = price_call("totally-made-up-model-xyz", 10_000, 10_000).usd;
        for &(id, _, _) in MODEL_PRICING {
            assert!(
                price_call(id, 10_000, 10_000).usd <= unknown + 1e-12,
                "{id} costs more than the unknown-model assumption"
            );
        }
    }

    #[test]
    fn catalog_hits_are_not_priced_as_assumed() {
        // Known model, bare alias, and dated snapshot all keep catalog rates —
        // only genuinely unmatched ids take the defensive path.
        for (model, expected_input) in [
            ("gpt-4o", 2.50),                  // exact catalog entry
            ("gpt-5.6", 5.00),                 // alias -> gpt-5.6-sol
            ("gpt-4o-2024-11-20", 2.50),       // dated snapshot of a family
            ("gpt-5.6-luna-2026-05-01", 0.20), // snapshot that must not degrade
        ] {
            let pricing = price_for_context(model, 0);
            assert!(!pricing.is_assumed(), "{model} took the defensive path");
            assert_eq!(
                pricing,
                ModelPricing::Catalog(pricing.price()),
                "{model} variant"
            );
            assert_eq!(pricing.price().input_per_1m, expected_input, "{model} rate");
            assert_eq!(pricing.catalog(), lookup(model), "{model} vs lookup()");
        }
        // A model that is merely a prefix of a table entry is still unknown.
        assert!(price_for_context("gpt-4", 0).is_assumed());
        assert!(price_for_context("gpt-4oxyz", 0).is_assumed());
        // Long-context tiers survive the rewrite.
        assert_eq!(
            price_for_context("gpt-5.6-sol", 300_000)
                .price()
                .input_per_1m,
            10.00
        );
        // As do scheduled changes, on both sides of the boundary.
        assert_eq!(
            price_at("claude-sonnet-5", 0, at("2026-08-11T00:00:00Z")),
            ModelPricing::Catalog(ModelPrice {
                input_per_1m: 2.00,
                output_per_1m: 10.00
            })
        );
        assert_eq!(
            price_at("claude-sonnet-5", 0, at("2026-09-01T00:00:00Z")),
            ModelPricing::Catalog(ModelPrice {
                input_per_1m: 3.00,
                output_per_1m: 15.00
            })
        );
    }

    #[test]
    fn lookup_still_reports_unknown_models_as_unpriced() {
        // `cost_cap`'s fail-closed check keys off `lookup(..).is_none()`, so
        // the defensive billing rate must NOT make unknown models look known.
        assert!(lookup("totally-made-up-model-xyz").is_none());
        assert!(estimate_request_cost("totally-made-up-model-xyz", 10, Some(10)).is_none());
    }

    #[test]
    fn anthropic_and_bedrock_priced() {
        assert!(lookup("claude-opus-4-8").is_some());
        assert!(lookup("anthropic.claude-haiku-4-5-20251001-v1:0").is_some());
    }

    /// A fixed instant inside Sonnet 5's introductory window, for tests that
    /// assert a rate which is scheduled to change (see [`SCHEDULED_PRICING`]).
    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn current_generation_models_priced() {
        // Current-generation ids must resolve to their own rates as published
        // on the official model pages, not fall back to an older family or $0.
        let luna = lookup("gpt-5.6-luna").unwrap();
        assert_eq!((luna.input_per_1m, luna.output_per_1m), (0.20, 1.20));
        // Terra/Sol must have their own rows — falling through to the shorter
        // "gpt-5" family prefix would misprice them.
        let terra = lookup("gpt-5.6-terra").unwrap();
        assert_eq!((terra.input_per_1m, terra.output_per_1m), (2.00, 12.00));
        let sol = lookup("gpt-5.6-sol").unwrap();
        assert_eq!((sol.input_per_1m, sol.output_per_1m), (5.00, 30.00));
        let sonnet5 = lookup_at("claude-sonnet-5", 0, at("2026-08-11T00:00:00Z")).unwrap();
        assert_eq!((sonnet5.input_per_1m, sonnet5.output_per_1m), (2.00, 10.00));
        let flash = lookup("gemini-3.6-flash").unwrap();
        assert_eq!((flash.input_per_1m, flash.output_per_1m), (1.50, 7.50));
        // Dated snapshots resolve to the same family.
        assert_eq!(
            lookup("gpt-5.6-luna-2026-05-01").unwrap().input_per_1m,
            0.20
        );
        assert_eq!(
            lookup_at("claude-sonnet-5-20260601", 0, at("2026-08-11T00:00:00Z"))
                .unwrap()
                .input_per_1m,
            2.00
        );
    }

    #[test]
    fn gpt_5_6_standard_rates_match_official_model_pages() {
        // developers.openai.com/api/docs/models/gpt-5.6-{luna,terra,sol},
        // read 2026-08-09. Exact values, so a silent drift fails here first.
        for (model, input, output) in [
            ("gpt-5.6-luna", 0.20, 1.20),
            ("gpt-5.6-terra", 2.00, 12.00),
            ("gpt-5.6-sol", 5.00, 30.00),
        ] {
            let p = lookup(model).unwrap();
            assert_eq!(
                (p.input_per_1m, p.output_per_1m),
                (input, output),
                "{model} standard rate"
            );
        }
    }

    #[test]
    fn gpt_5_6_snapshots_never_select_the_older_gpt_5_row() {
        // `gpt-5` is a real (cheaper) row and a prefix of every 5.6 id at a
        // version boundary. Longest-family matching must keep 5.6 snapshots on
        // their own row; falling back would under-reserve predictive admission.
        let gpt5 = lookup("gpt-5").unwrap();
        assert_eq!((gpt5.input_per_1m, gpt5.output_per_1m), (1.25, 10.00));
        for (model, expected) in [
            ("gpt-5.6-luna-2026-05-01", lookup("gpt-5.6-luna").unwrap()),
            ("gpt-5.6-terra-2026-05-01", lookup("gpt-5.6-terra").unwrap()),
            ("gpt-5.6-sol-2026-05-01", lookup("gpt-5.6-sol").unwrap()),
            ("gpt-5.6", lookup("gpt-5.6-sol").unwrap()),
        ] {
            let p = lookup(model).unwrap();
            assert_eq!(p, expected, "{model} must not degrade to the gpt-5 row");
            assert_ne!(p, gpt5, "{model} resolved to the older gpt-5 row");
        }
    }

    #[test]
    fn sonnet_5_introductory_rate_ends_on_its_own_date() {
        // The introductory rate is published as "$2 / $10 per MTok through
        // August 31, 2026 (standard $3 / $15 thereafter)". The transition must
        // happen from the table, not from an emergency edit on September 1.
        let intro = (2.00, 10.00);
        let standard = (3.00, 15.00);
        for (instant, expected) in [
            ("2026-08-11T00:00:00Z", intro),
            ("2026-08-31T23:59:59Z", intro),
            ("2026-09-01T00:00:00Z", standard),
            ("2026-09-01T00:00:01Z", standard),
            ("2027-01-01T00:00:00Z", standard),
        ] {
            let p = lookup_at("claude-sonnet-5", 0, at(instant)).unwrap();
            assert_eq!(
                (p.input_per_1m, p.output_per_1m),
                expected,
                "claude-sonnet-5 @ {instant}"
            );
            // Dated snapshots of the family follow the same schedule.
            let snap = lookup_at("claude-sonnet-5-20260601", 0, at(instant)).unwrap();
            assert_eq!(snap, p, "snapshot @ {instant}");
        }
        // The boundary constant really is 2026-09-01T00:00:00Z.
        assert_eq!(
            SONNET_5_STANDARD_FROM,
            at("2026-09-01T00:00:00Z").timestamp()
        );
        // Models with no scheduled change are unaffected by the instant.
        assert_eq!(
            lookup_at("gpt-4o", 0, at("2020-01-01T00:00:00Z")),
            lookup_at("gpt-4o", 0, at("2030-01-01T00:00:00Z"))
        );
    }

    #[test]
    fn scheduled_models_have_no_long_context_tier() {
        // `lookup_at` returns a long-context tier before consulting
        // `SCHEDULED_PRICING`, so a scheduled model that also carried a tier
        // row would silently ignore its own price change above the threshold.
        // No such model exists today; if one appears, make the tier table
        // date-aware rather than deleting this test.
        for &(scheduled_id, _, _, _) in SCHEDULED_PRICING {
            assert!(
                !LONG_CONTEXT_PRICING
                    .iter()
                    .any(|&(tier_id, _, _, _)| tier_id == scheduled_id),
                "{scheduled_id} has both a scheduled price change and a long-context tier"
            );
        }
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
            ("gpt-5.6-luna", (0.20, 1.20), (0.40, 1.80)),
            ("gpt-5.6-terra", (2.00, 12.00), (4.00, 18.00)),
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
        assert_eq!((base.input_per_1m, base.output_per_1m), (0.20, 1.20));
        // Above it: the documented high-context rates (2x in / 1.5x out).
        let hi = lookup_for_context("gpt-5.6-luna", 272_001).unwrap();
        assert_eq!((hi.input_per_1m, hi.output_per_1m), (0.40, 1.80));
        // Dated snapshots of a tiered family get the tier too.
        let hi_snap = lookup_for_context("gpt-5.6-luna-2026-05-01", 300_000).unwrap();
        assert_eq!(hi_snap.input_per_1m, 0.40);
        // Gemini 2.5 Pro doubles above 200K prompt tokens.
        let g = lookup_for_context("gemini-2.5-pro", 250_000).unwrap();
        assert_eq!((g.input_per_1m, g.output_per_1m), (2.50, 15.00));
        // Models without a documented tier keep flat rates at any size.
        let flat = lookup_for_context("gpt-4o", 5_000_000).unwrap();
        assert_eq!(flat.input_per_1m, 2.50);
        // estimate_cost and estimate_request_cost pick the tier as well.
        let c = estimate_cost("gpt-5.6-luna", 300_000, 1000);
        let expected = (300_000.0 / 1e6) * 0.40 + (1000.0 / 1e6) * 1.80;
        assert!((c - expected).abs() < 1e-9);
        let r = estimate_request_cost("gpt-5.6-luna", 300_000, Some(1000)).unwrap();
        assert!((r - expected).abs() < 1e-9);
    }

    #[test]
    fn long_context_estimates_at_300k_match_published_rates() {
        // 300K input + 1K output, priced off the official >272K tier. Absolute
        // figures (not derived from the same constants under test) so a wrong
        // base row can't produce a self-consistent wrong answer.
        for (model, expected) in [
            ("gpt-5.6-luna", 0.1218),
            ("gpt-5.6-terra", 1.2180),
            ("gpt-5.6-sol", 3.0450),
        ] {
            let c = estimate_cost(model, 300_000, 1_000);
            assert!(
                (c - expected).abs() < 1e-9,
                "{model}: estimated {c}, expected {expected}"
            );
        }
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
