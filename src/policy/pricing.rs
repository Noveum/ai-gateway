//! Provider-aware cost model: rate lookup plus a billing-complete
//! [`CostBreakdown`].
//!
//! Rates come from [`crate::policy::pricing_catalog`], a hand-maintained table
//! matching `pricing/catalog.json`. The platform's TypeScript table carries the
//! same rates and has to be kept in step with it by hand.
//!
//! # Why a breakdown and not one number
//!
//! A flat input/output table cannot express what providers actually bill. A
//! cached prompt read costs 0.02x–0.5x of uncached input depending on the model;
//! a cache *write* costs 1.25x–2x; tool and search calls carry per-request fees
//! that no token count reflects. Pricing those with an input/output table means
//! every dimension it cannot see is charged at $0 — silently. The whole point of
//! [`CostBreakdown`] is that a dimension the catalog cannot price is *named*
//! ([`CostBreakdown::missing_dimensions`]) and charged at a conservative upper
//! bound, never dropped. `is_complete == false` is the signal that a cost is a
//! bound rather than a quote. [`CostBreakdown::blocks_fail_closed`] exposes the
//! conservative decision a caller can apply; the current policy preflight uses
//! `failClosed` for unknown model lookups, while known-model missing dimensions
//! remain flagged and conservatively metered.
//!
//! # Layering
//!
//! * [`lookup`] / [`lookup_at`] — catalog-only rates; `None` for an unknown
//!   model. `cost_cap`'s fail-closed check keys off this.
//! * [`price_at`] / [`price_call`] — always yields rates, tagging an unknown
//!   model [`ModelPricing::UnknownAssumed`] so it can never meter at $0.
//! * [`price_usage`] — the full breakdown across every billable dimension.
//! * [`estimate_cost`] — the provider extractors' catalog-only entry point,
//!   which reads `0.0` as "unpriced". Kept deliberately narrow; the metering
//!   boundary backfills the defensive estimate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

use crate::policy::pricing_catalog as catalog;

pub use crate::policy::pricing_catalog::CATALOG_VERSION;

#[derive(Debug, Clone, PartialEq)]
struct BedrockAnthropicPricing {
    catalog_model: String,
    token_price_multiplier: f64,
}

/// Resolve an Anthropic Bedrock foundation model from either its direct id or
/// a system-defined inference-profile id/ARN. AWS application inference
/// profiles are deliberately excluded: their opaque id does not identify the
/// backing model without a control-plane lookup.
fn bedrock_anthropic_pricing(model: &str) -> Option<BedrockAnthropicPricing> {
    fn system_profile_id(arn: &str) -> Option<&str> {
        let mut parts = arn.splitn(6, ':');
        if parts.next()? != "arn" {
            return None;
        }
        // The catalog holds commercial AWS rates. GovCloud, China and ISO
        // partitions publish separate regional prices, so their ARNs must stay
        // unknown until those rows exist rather than borrowing this multiplier.
        if parts.next()? != "aws" || parts.next()? != "bedrock" {
            return None;
        }
        let _region = parts.next()?;
        let account = parts.next()?;
        if !account.is_empty()
            && (account.len() != 12 || !account.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return None;
        }
        let id = parts.next()?.strip_prefix("inference-profile/")?;
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.'))
        {
            return None;
        }
        Some(id)
    }

    let lower = model.to_ascii_lowercase();
    let profile_id = if lower.starts_with("arn:") {
        system_profile_id(&lower)?
    } else {
        lower.as_str()
    };
    let (catalog_id, explicitly_global) = match profile_id.split_once('.') {
        Some(("global", underlying)) => (underlying, true),
        Some(("us" | "eu" | "au" | "jp" | "apac", underlying)) => (underlying, false),
        _ => (profile_id, false),
    };
    let row = catalog::MODEL_ROWS.iter().find(|row| {
        row.provider == "bedrock" && row.id.starts_with("anthropic.") && row.id == catalog_id
    })?;
    Some(BedrockAnthropicPricing {
        catalog_model: row.id.to_string(),
        token_price_multiplier: if explicitly_global { 1.0 } else { 1.1 },
    })
}

/// Whether a Bedrock model/profile has a complete strict-admission rate that
/// can be derived from the request model id alone.
///
/// AWS prices Amazon Nova and other Bedrock families by source region, but the
/// current admission/settlement API does not carry `x-aws-region`. Until that
/// context is threaded into pricing, only catalogued commercial Claude model
/// and system-profile ids have a safe global/geographic rate derivation.
pub fn bedrock_model_has_strict_pricing(model: &str) -> bool {
    bedrock_anthropic_pricing(model).is_some()
}

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

/// Normalize a caller-supplied model id: lowercase, resolve a safely
/// identifiable Bedrock inference profile to its foundation model, then
/// resolve a bare provider alias (e.g. `gpt-5.6` → `gpt-5.6-sol`) to the
/// concrete model it routes to. Used by every lookup so alias requests price
/// and tier exactly like the model that will actually serve them.
fn canonical(model: &str) -> String {
    let m = model.to_lowercase();
    let m = bedrock_anthropic_pricing(&m)
        .map(|pricing| pricing.catalog_model)
        .unwrap_or(m);
    match catalog::MODEL_ALIASES.iter().find(|(alias, _)| *alias == m) {
        Some((_, target)) => (*target).to_string(),
        None => m,
    }
}

/// Pricing for one model: USD per 1M input/output tokens.
///
/// The two-dimension view, kept for the admission and cap paths that only ever
/// reason about input and output. [`RateCard`] is the full picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

/// Every rate a request can be billed against, USD per 1M tokens.
///
/// `None` on a cache field means the provider publishes **no rate** for that
/// dimension, which is different from `Some(0.0)` ("documented as free"). The
/// distinction is the whole mechanism: `None` produces a missing dimension and a
/// conservative charge, `Some(0.0)` produces an honest zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateCard {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    /// Prompt-cache HIT rate.
    pub cached_input_per_1m: Option<f64>,
    /// Cache-write rate at the provider's default TTL.
    pub cache_write_per_1m: Option<f64>,
    /// Anthropic's 1-hour-TTL cache write. `None` where a provider publishes a
    /// single write rate.
    pub cache_write_1h_per_1m: Option<f64>,
}

impl RateCard {
    /// The two-dimension view.
    pub fn model_price(self) -> ModelPrice {
        ModelPrice {
            input_per_1m: self.input_per_1m,
            output_per_1m: self.output_per_1m,
        }
    }
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

// ---------------------------------------------------------------------------
// Billable dimensions
// ---------------------------------------------------------------------------

/// One axis a provider bills on. Naming them is what makes an incomplete cost
/// auditable: the usage record says *which* part of the bill the gateway could
/// not compute, rather than reporting a total that quietly omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BillableDimension {
    /// Prompt tokens billed at the standard input rate.
    UncachedInput,
    /// Prompt tokens served from the provider's prompt cache (a cache HIT).
    CacheRead,
    /// Prompt tokens written INTO the cache, billed at a premium.
    CacheWrite,
    /// Completion tokens.
    Output,
    /// Per-request tool and search fees (web search, file search, grounding).
    Tool,
}

impl BillableDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            BillableDimension::UncachedInput => "UNCACHED_INPUT",
            BillableDimension::CacheRead => "CACHE_READ",
            BillableDimension::CacheWrite => "CACHE_WRITE",
            BillableDimension::Output => "OUTPUT",
            BillableDimension::Tool => "TOOL",
        }
    }
}

/// Where a total came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CostSource {
    /// The provider returned a billed total the gateway is configured to trust
    /// (see [`catalog::AUTHORITATIVE_COST_PROVIDERS`]). It supersedes the
    /// gateway's own arithmetic.
    ProviderReported,
    /// Computed dimension by dimension from the catalog.
    Catalog,
}

/// A fully itemized cost for one call.
///
/// `total_usd` is always the amount to bill; the components explain it. When
/// `is_complete` is `false` the total is a **conservative upper bound**, not a
/// quote: at least one dimension carried tokens the catalog had no rate for, and
/// those tokens were charged at the bound described on
/// [`conservative_cache_read_rate`] / [`conservative_cache_write_rate`] rather
/// than dropped. That is the invariant this type exists to enforce — a missing
/// dimension can never produce a silent $0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub uncached_input_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub output_usd: f64,
    pub tool_usd: f64,
    pub total_usd: f64,
    /// `true` when every dimension that carried usage had a published rate.
    pub is_complete: bool,
    /// The dimensions that did not, in a stable order.
    pub missing_dimensions: Vec<BillableDimension>,
    /// The rate card version that produced these numbers.
    ///
    /// Owned rather than `&'static str` because a breakdown round-trips through
    /// the telemetry log and the platform's usage API, and a deserialized one
    /// carries whatever version *that* record was priced at, which may not be
    /// the version this binary was built with.
    pub pricing_version: String,
    /// `true` when the model matched nothing in the catalog and was priced at
    /// the assumed maximum rate.
    pub assumed_model_rate: bool,
    pub source: CostSource,
}

impl CostBreakdown {
    /// Whether a fail-closed cost policy must refuse this call.
    ///
    /// A fail-closed policy is a promise that spend cannot exceed a cap. That
    /// promise is only keepable when the spend can be computed, so an
    /// incomplete breakdown — or one resting on the unknown-model assumption —
    /// is a block. A fail-open policy uses the conservative total instead
    /// ([`CostBreakdown::total_usd`] is safe to meter in both cases).
    pub fn blocks_fail_closed(&self) -> bool {
        !self.is_complete || self.assumed_model_rate
    }

    /// The missing dimensions as wire strings, for logs and usage records.
    pub fn missing_dimension_names(&self) -> Vec<&'static str> {
        self.missing_dimensions.iter().map(|d| d.as_str()).collect()
    }

    fn record_missing(&mut self, dim: BillableDimension) {
        self.is_complete = false;
        if !self.missing_dimensions.contains(&dim) {
            self.missing_dimensions.push(dim);
        }
    }
}

/// The usage one call consumed, split by billable dimension.
///
/// Token counts are **disjoint**: `uncached_input_tokens` excludes cache reads
/// and cache writes. Providers disagree about whether their own prompt-token
/// field is inclusive, which is exactly the trap [`parse_usage`] exists to
/// normalize away.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BillableUsage {
    pub uncached_input_tokens: u32,
    pub cache_read_tokens: u32,
    /// Cache writes at the provider's default TTL.
    pub cache_write_tokens: u32,
    /// Cache writes explicitly declared at Anthropic's 1-hour TTL.
    pub cache_write_1h_tokens: u32,
    pub output_tokens: u32,
    /// `(tool_id, call_count)`, ids as in `catalog::TOOL_FEES_USD_PER_1K_CALLS`.
    pub tool_calls: Vec<(String, u32)>,
    /// Request/response-selected multiplier for token and cache dimensions.
    /// `None` is the standard 1.0 rate. Anthropic US-only data residency
    /// contributes 1.1x and fast mode 2x; both stack multiplicatively.
    pub token_price_multiplier: Option<f64>,
    /// The provider explicitly says this usage shape is not billed. Anthropic
    /// pre-output refusals are HTTP 200 and retain token counts for observability
    /// even though their monetary cost is zero.
    pub billing_exempt: bool,
    /// A billed total the provider returned, accepted only from a provider on
    /// [`catalog::AUTHORITATIVE_COST_PROVIDERS`].
    pub provider_reported_cost_usd: Option<f64>,
}

impl BillableUsage {
    /// The legacy two-dimension view: plain input and output, no cache, no
    /// tools. What the token-only call sites still have.
    pub fn from_tokens(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            uncached_input_tokens: input_tokens,
            output_tokens,
            ..Default::default()
        }
    }

    /// Every prompt token, cached or not. This is what selects a long-context
    /// tier: providers tier on the size of the whole request, and a cached
    /// prefix still occupies the context window.
    pub fn total_input_tokens(&self) -> u32 {
        self.uncached_input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.cache_write_1h_tokens)
    }

    /// Whether any dimension carried usage at all.
    pub fn is_empty(&self) -> bool {
        self.total_input_tokens() == 0 && self.output_tokens == 0 && self.tool_calls.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Defensive rates
// ---------------------------------------------------------------------------

/// USD per 1M tokens charged to a model that matches nothing in the catalog.
///
/// **Derived from the catalog rather than hard-coded**: the maximum input rate
/// and the maximum output rate over every Rust runtime row — base, announced
/// and long-context. Today input is bounded by `o1` at $15.00/M and output by
/// `gpt-5.6-cyber` at $75.00/M. Adding a pricier runtime row automatically
/// raises the assumption, so an unknown model can never be estimated below a
/// model the compiled gateway knows how to price
/// (`assumed_rate_bounds_every_catalog_rate` pins this).
///
/// Erring high is the intended direction. An unknown id is either a newly
/// released model — historically priced at or near the top of the market — or
/// a typo. Over-estimating a typo costs a request that gets blocked slightly
/// early and an alertable warning; under-estimating a real model leaks spend
/// past every cost cap silently, which is exactly the bug this replaces.
///
/// NOV-135, decided. Two questions were open here; both are now settled.
///
/// **The rate stays the derived catalog maximum.** It is not invented, it rises
/// automatically with the catalog, and it errs in the only safe direction:
/// over-estimating a typo costs one early block plus an alertable warning, while
/// under-estimating a real model leaks spend past every cap silently. The
/// complaint that drove this — Groq streams metering ~50x high — was never the
/// assumption's fault. Those streams arrived carrying a *placeholder* id that no
/// event had ever populated, and the cure was to resolve it from the request
/// (`telemetry::middleware::resolve_model_for_metering`), not to lower the
/// assumption for every genuinely unknown model.
///
/// **An unknown model is priced high, never rejected.** This gateway is a
/// pass-through proxy; refusing an id it does not recognize would turn every
/// provider model launch into an outage on a component that has no business
/// having an opinion about which models exist. Operators who *do* want an
/// unmeterable call refused already have a per-policy lever that is strictly
/// better than a global switch: a `cost_cap` with `failClosed: true` blocks a
/// model it cannot meter. Strictness belongs to the policy, not the proxy.
pub fn assumed_unknown_price() -> ModelPrice {
    static ASSUMED: OnceLock<ModelPrice> = OnceLock::new();
    *ASSUMED.get_or_init(|| {
        let rows = catalog::MODEL_ROWS
            .iter()
            .map(|r| (r.input_per_1m, r.output_per_1m))
            .chain(
                catalog::SCHEDULED_ROWS
                    .iter()
                    .map(|r| (r.input_per_1m, r.output_per_1m)),
            )
            .chain(
                catalog::LONG_CONTEXT_ROWS
                    .iter()
                    .map(|r| (r.input_per_1m, r.output_per_1m)),
            );
        let (input_per_1m, output_per_1m) = rows.fold((0.0_f64, 0.0_f64), |(mi, mo), (i, o)| {
            (mi.max(i), mo.max(o))
        });
        ModelPrice {
            input_per_1m,
            output_per_1m,
        }
    })
}

/// Apply a scheduled base-rate change, carrying the cache dimensions with it.
///
/// Providers publish cache rates as multiples of base input (Anthropic: 0.1x
/// read, 1.25x/2x write), so a base change has to rescale them by the same
/// ratio. Leaving them behind would bill a cache read at the superseded rate
/// indefinitely, and cache reads are the majority of input tokens on any
/// prompt-cached workload.
///
/// Kept as a free function so the machinery stays under test even when the
/// scheduled table is empty, which it currently is.
fn rescale_for_scheduled(row: &catalog::CatalogRow, sched: &catalog::ScheduledRow) -> RateCard {
    let ratio = if row.input_per_1m > 0.0 {
        sched.input_per_1m / row.input_per_1m
    } else {
        1.0
    };
    RateCard {
        input_per_1m: sched.input_per_1m,
        output_per_1m: sched.output_per_1m,
        cached_input_per_1m: row.cached_input_per_1m.map(|v| v * ratio),
        cache_write_per_1m: row.cache_write_per_1m.map(|v| v * ratio),
        cache_write_1h_per_1m: row.cache_write_1h_per_1m.map(|v| v * ratio),
    }
}

/// The largest cache-write premium any catalogued provider charges, as a
/// multiple of that model's uncached input rate. Derived, not hard-coded:
/// today it is Anthropic's 1-hour TTL at 2x. It is the upper bound used when a
/// model reports cache-write tokens but publishes no write rate.
pub fn max_cache_write_multiplier() -> f64 {
    static MAX: OnceLock<f64> = OnceLock::new();
    *MAX.get_or_init(|| {
        catalog::MODEL_ROWS
            .iter()
            .filter(|r| r.input_per_1m > 0.0)
            .flat_map(|r| {
                [r.cache_write_per_1m, r.cache_write_1h_per_1m]
                    .into_iter()
                    .flatten()
                    .map(|w| w / r.input_per_1m)
            })
            .fold(1.0_f64, f64::max)
    })
}

/// The largest per-request tool fee in the catalog, USD per 1000 calls. Used
/// for a tool id the catalog does not know, so an unrecognized search fee is
/// over-charged and flagged rather than billed at zero.
pub fn max_tool_fee_per_1k_calls() -> f64 {
    static MAX: OnceLock<f64> = OnceLock::new();
    *MAX.get_or_init(|| {
        catalog::TOOL_FEES_USD_PER_1K_CALLS
            .iter()
            .fold(0.0_f64, |m, &(_, fee)| m.max(fee))
    })
}

/// The conservative rate for a cache READ whose true rate is unpublished: the
/// model's own uncached input rate.
///
/// Safe as an upper bound because a cache hit is a *discount* everywhere it is
/// published (0.02x at DeepSeek, 0.1x at Anthropic/OpenAI's newer families,
/// 0.5x at gpt-4o). No provider bills a cache read above uncached input, so
/// charging uncached input can only over-estimate.
pub fn conservative_cache_read_rate(input_per_1m: f64) -> f64 {
    input_per_1m
}

/// The conservative rate for a cache WRITE whose true rate is unpublished: the
/// model's uncached input rate times [`max_cache_write_multiplier`].
///
/// Cache writes are a *premium*, so unlike reads the input rate alone would
/// under-charge. The multiplier is the largest one any catalogued provider
/// publishes, which makes this an upper bound over the known market.
pub fn conservative_cache_write_rate(input_per_1m: f64) -> f64 {
    input_per_1m * max_cache_write_multiplier()
}

/// Distinct model ids to remember before resetting the warn-dedup set.
/// Bounded because model ids come from untrusted request bodies: an unbounded
/// set would grow without limit, and warning on *every* lookup would let one
/// hot unknown model flood the log. On overflow the set clears, so a persistent
/// problem re-warns periodically instead of going quiet forever.
const UNKNOWN_MODEL_WARN_CAPACITY: usize = 256;

/// The single deduped alert channel for "this call was not priced from
/// published rates" (NOV-152, extended for NOV-158's missing dimensions).
///
/// Both causes flow through here so one hot model cannot flood the log and so
/// there is exactly one place to point an alert at. The dedup key includes the
/// reason, so a model that first appears with an unknown id and later with an
/// unpriceable cache dimension warns once for each.
fn warn_incomplete_pricing(canonical_id: &str, key: &str, emit: impl FnOnce()) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.len() >= UNKNOWN_MODEL_WARN_CAPACITY {
        seen.clear();
    }
    let deduped = format!("{canonical_id}|{key}");
    if !seen.insert(deduped) {
        return false;
    }
    drop(seen);
    emit();
    true
}

/// Emit one alertable event per distinct unknown model id.
fn warn_unknown_model(model: &str, canonical_id: &str, assumed: ModelPrice) {
    let _emitted = warn_incomplete_pricing(canonical_id, "unknown-model", || {
        warn!(
            model = %model,
            canonical_model = %canonical_id,
            pricing_version = %CATALOG_VERSION,
            assumed_input_usd_per_1m = assumed.input_per_1m,
            assumed_output_usd_per_1m = assumed.output_per_1m,
            "Nova Guard: no pricing entry for model; billing at the assumed maximum catalog rate"
        );
    });
}

/// Emit one alertable event per distinct (model, missing dimension) pair.
fn warn_missing_dimension(model: &str, canonical_id: &str, dim: BillableDimension, charged: f64) {
    let _emitted = warn_incomplete_pricing(canonical_id, dim.as_str(), || {
        warn!(
            model = %model,
            canonical_model = %canonical_id,
            pricing_version = %CATALOG_VERSION,
            dimension = dim.as_str(),
            conservative_usd_per_1m = charged,
            "Nova Guard: model reported usage on a billable dimension the catalog cannot price; \
             charging a conservative upper bound and marking the cost incomplete"
        );
    });
}

// ---------------------------------------------------------------------------
// Rate resolution
// ---------------------------------------------------------------------------

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

/// Whether Anthropic can select US-only inference (and therefore the 1.1x
/// token/cache multiplier) for this model. Unknown future Claude ids are
/// treated as eligible so a newly launched model cannot under-reserve; known
/// pre-4.6 catalog rows remain at their exact standard rate.
pub fn anthropic_supports_inference_geo(model: &str) -> bool {
    let model = canonical(model);
    if model.is_empty() {
        return false;
    }
    let supported = [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
    ]
    .iter()
    .any(|family| is_family(&model, family));
    if supported {
        return true;
    }
    // A known Anthropic row not listed above predates 4.6 and does not accept
    // `inference_geo`. Anything else may be a future pass-through model, for
    // which the conservative 1.1x reservation is the safe contract.
    !catalog::MODEL_ROWS
        .iter()
        .any(|row| row.provider == "anthropic" && is_family(&model, row.id))
}

/// The catalog row a canonical id resolves to: exact match wins, otherwise the
/// longest family the id extends at a version boundary.
fn match_row(m: &str) -> Option<&'static catalog::CatalogRow> {
    if let Some(row) = catalog::MODEL_ROWS.iter().find(|r| r.id == m) {
        return Some(row);
    }
    let mut best: Option<&'static catalog::CatalogRow> = None;
    for row in catalog::MODEL_ROWS {
        if m.len() > row.id.len()
            && m.starts_with(row.id)
            && matches!(m.as_bytes()[row.id.len()], b'-' | b':' | b'.' | b'@' | b'/')
            && best.is_none_or(|b| row.id.len() > b.id.len())
        {
            best = Some(row);
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

/// Resolve every rate for a model at a given request size and instant.
///
/// Order of precedence, matching how providers bill:
/// 1. a documented long-context tier, when the request exceeds its threshold —
///    it prices the *whole* request and supersedes the base row;
/// 2. otherwise the base row, with the latest effective scheduled change
///    applied.
///
/// A scheduled change publishes new base rates only. Cache rates are published
/// as multiples of base input (Anthropic states them that way explicitly), so
/// the cache dimensions are rescaled by the same ratio rather than left on the
/// superseded base — leaving them stale would bill a cache read at the old rate
/// after the model's price changed.
fn resolve_rates_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<RateCard> {
    let m = canonical(model);
    let row = match_row(&m)?;

    for tier in catalog::LONG_CONTEXT_ROWS {
        if is_family(&m, tier.id) && input_tokens > tier.threshold_input_tokens {
            return Some(RateCard {
                input_per_1m: tier.input_per_1m,
                output_per_1m: tier.output_per_1m,
                cached_input_per_1m: tier.cached_input_per_1m,
                cache_write_per_1m: tier.cache_write_per_1m,
                cache_write_1h_per_1m: None,
            });
        }
    }

    let mut card = RateCard {
        input_per_1m: row.input_per_1m,
        output_per_1m: row.output_per_1m,
        cached_input_per_1m: row.cached_input_per_1m,
        cache_write_per_1m: row.cache_write_per_1m,
        cache_write_1h_per_1m: row.cache_write_1h_per_1m,
    };

    let mut effective_from = i64::MIN;
    for sched in catalog::SCHEDULED_ROWS {
        if sched.id == row.id
            && at.timestamp() >= sched.effective_from_unix_secs
            && sched.effective_from_unix_secs > effective_from
        {
            effective_from = sched.effective_from_unix_secs;
            card = rescale_for_scheduled(row, sched);
        }
    }
    Some(card)
}

/// The catalog rates for a model, or `None` when nothing matches. The shared
/// body of [`lookup_at`] (advisory: "do we know this model?") and [`price_at`]
/// (billing: "what do we charge?"), so the two can never disagree about which
/// ids are known. Deliberately silent — [`price_at`] owns the warning, so
/// advisory lookups don't double-log every priced request.
fn resolve_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<ModelPrice> {
    resolve_rates_at(model, input_tokens, at).map(RateCard::model_price)
}

/// Look up pricing for a model id.
///
/// Matching is case-insensitive: an exact match wins; otherwise the input may be
/// a *dated snapshot* of a known family (e.g. `gpt-4o-2024-11-20` → `gpt-4o`), in
/// which case the longest table id that is a prefix of the input wins, but only
/// when the next character is a version separator (`-`, `:`, `.`, `@`, `/`) so
/// that `gpt-4` cannot match `gpt-4o`. A short or garbage id that is merely a
/// prefix of a table entry returns `None` (never the other direction).
///
/// Bare provider aliases resolve to their concrete target first, so `gpt-5.6`
/// prices as Sol rather than as a snapshot of `gpt-5`.
pub fn lookup(model: &str) -> Option<ModelPrice> {
    lookup_at(model, 0, now())
}

/// Look up pricing for a model at a given input size, **as of a given
/// instant** — applying any scheduled change that has taken effect by `at`,
/// then the model's documented long-context tier when `input_tokens` exceeds
/// its threshold.
pub fn lookup_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<ModelPrice> {
    resolve_at(model, input_tokens, at)
}

/// Look up pricing for a model at a given input size, at the current time.
pub fn lookup_for_context(model: &str, input_tokens: u32) -> Option<ModelPrice> {
    lookup_at(model, input_tokens, now())
}

/// Every rate for a model at a given input size and instant, or `None` when the
/// model matches nothing in the catalog.
pub fn rates_at(model: &str, input_tokens: u32, at: DateTime<Utc>) -> Option<RateCard> {
    resolve_rates_at(model, input_tokens, at)
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
/// model is not left free: `telemetry::middleware` backfills [`price_usage`]'s
/// defensive breakdown at the metering boundary, before the usage event that
/// cost caps and billing read. Prefer [`price_usage`] in new code — it makes
/// both the unknown model and the unpriceable dimension impossible to overlook.
pub fn estimate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    match lookup_for_context(model, input_tokens) {
        Some(p) => {
            (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
                + (output_tokens as f64 / 1_000_000.0) * p.output_per_1m
        }
        None => 0.0,
    }
}

// ---------------------------------------------------------------------------
// The breakdown
// ---------------------------------------------------------------------------

/// Price one call across every billable dimension, at the current time.
pub fn price_usage(model: &str, usage: &BillableUsage) -> CostBreakdown {
    price_usage_at(model, usage, now())
}

/// Price one call across every billable dimension, as of a given instant.
///
/// * A provider-reported total is preferred when it is authoritative — the
///   provider is the system of record for its own bill, and its number already
///   includes dimensions the gateway may not model. Components are still
///   itemized from the catalog for auditability.
/// * Otherwise every dimension is priced from the resolved [`RateCard`].
/// * A dimension that carried tokens but has no published rate is charged at a
///   conservative upper bound and recorded in
///   [`CostBreakdown::missing_dimensions`]; the result is `is_complete: false`.
///   It is never dropped, so an incomplete cost cannot post as $0.
pub fn price_usage_at(model: &str, usage: &BillableUsage, at: DateTime<Utc>) -> CostBreakdown {
    let pricing = price_at(model, usage.total_input_tokens(), at);
    let assumed_model_rate = pricing.is_assumed();
    let card = resolve_rates_at(model, usage.total_input_tokens(), at).unwrap_or_else(|| {
        // Unknown model: the assumed maximum rate, with both cache dimensions
        // unpublished by construction so any cache usage on it is also flagged.
        let p = assumed_unknown_price();
        RateCard {
            input_per_1m: p.input_per_1m,
            output_per_1m: p.output_per_1m,
            cached_input_per_1m: None,
            cache_write_per_1m: None,
            cache_write_1h_per_1m: None,
        }
    });

    let mut out = CostBreakdown {
        uncached_input_usd: 0.0,
        cache_read_usd: 0.0,
        cache_write_usd: 0.0,
        output_usd: 0.0,
        tool_usd: 0.0,
        total_usd: 0.0,
        is_complete: true,
        missing_dimensions: Vec::new(),
        pricing_version: CATALOG_VERSION.to_string(),
        assumed_model_rate,
        source: CostSource::Catalog,
    };
    let canonical_id = canonical(model);
    let per_1m = |tokens: u32, rate: f64| (tokens as f64 / 1_000_000.0) * rate;

    let token_multiplier = usage
        .token_price_multiplier
        .filter(|multiplier| multiplier.is_finite() && *multiplier >= 1.0)
        .unwrap_or(1.0);
    out.uncached_input_usd =
        per_1m(usage.uncached_input_tokens, card.input_per_1m) * token_multiplier;
    out.output_usd = per_1m(usage.output_tokens, card.output_per_1m) * token_multiplier;

    if usage.cache_read_tokens > 0 {
        let rate = match card.cached_input_per_1m {
            Some(r) => r,
            None => {
                let bound = conservative_cache_read_rate(card.input_per_1m);
                out.record_missing(BillableDimension::CacheRead);
                warn_missing_dimension(model, &canonical_id, BillableDimension::CacheRead, bound);
                bound
            }
        };
        out.cache_read_usd = per_1m(usage.cache_read_tokens, rate) * token_multiplier;
    }

    if usage.cache_write_tokens > 0 || usage.cache_write_1h_tokens > 0 {
        let default_rate = card.cache_write_per_1m;
        let hour_rate = card.cache_write_1h_per_1m.or(default_rate);
        if default_rate.is_none() || (usage.cache_write_1h_tokens > 0 && hour_rate.is_none()) {
            let bound = conservative_cache_write_rate(card.input_per_1m);
            out.record_missing(BillableDimension::CacheWrite);
            warn_missing_dimension(model, &canonical_id, BillableDimension::CacheWrite, bound);
            out.cache_write_usd = per_1m(
                usage
                    .cache_write_tokens
                    .saturating_add(usage.cache_write_1h_tokens),
                bound,
            ) * token_multiplier;
        } else {
            out.cache_write_usd = per_1m(usage.cache_write_tokens, default_rate.unwrap_or(0.0))
                * token_multiplier
                + per_1m(usage.cache_write_1h_tokens, hour_rate.unwrap_or(0.0)) * token_multiplier;
        }
    }

    for (tool_id, calls) in &usage.tool_calls {
        if *calls == 0 {
            continue;
        }
        let fee = catalog::TOOL_FEES_USD_PER_1K_CALLS
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(tool_id))
            .map(|&(_, fee)| fee);
        let rate = match fee {
            Some(f) => f,
            None => {
                let bound = max_tool_fee_per_1k_calls();
                out.record_missing(BillableDimension::Tool);
                warn_missing_dimension(model, &canonical_id, BillableDimension::Tool, bound);
                bound
            }
        };
        out.tool_usd += (*calls as f64 / 1000.0) * rate;
    }

    out.total_usd = out.uncached_input_usd
        + out.cache_read_usd
        + out.cache_write_usd
        + out.output_usd
        + out.tool_usd;

    if usage.billing_exempt {
        out.uncached_input_usd = 0.0;
        out.cache_read_usd = 0.0;
        out.cache_write_usd = 0.0;
        out.output_usd = 0.0;
        out.tool_usd = 0.0;
        out.total_usd = 0.0;
    }

    // The provider is the system of record for its own bill. When we trust its
    // total it replaces our arithmetic outright, and nothing is missing any
    // more: the components stay for auditability but stop being the answer.
    if let Some(reported) = usage.provider_reported_cost_usd {
        if reported.is_finite() && reported >= 0.0 {
            out.total_usd = reported;
            out.source = CostSource::ProviderReported;
            out.is_complete = true;
            out.missing_dimensions.clear();
        }
    }
    out
}

/// Assumed completion size when a request does not set an output limit. This is
/// an advisory/rate-only estimate and an outage fallback; an enforcing strict
/// cost cap rejects an unbounded request instead of pretending this heuristic
/// is a hard maximum. Operators can tune the estimate via
/// `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS`.
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
/// This is the catalog-only signal. Callers that must reserve an amount should
/// use [`reserve_request_cost`], which never returns `None`.
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

/// The amount to reserve for a request before forwarding it. **Always yields a
/// figure**, falling back to [`assumed_unknown_price`] for a model the catalog
/// does not know.
///
/// NOV-135, decided. Admission used to return `None` for an unknown model and
/// every caller read that as `0.0`, so an unknown model reserved **nothing**
/// while [`price_usage`] billed it at the catalog maximum on the way out. A cost
/// cap could therefore be walked straight past by naming a model the gateway had
/// never heard of — the same class of hole as metering a stream at its estimate,
/// and reachable by anyone who can pick a model id.
///
/// Reserving and billing now rest on the same assumption, so a cap sees an
/// unknown model coming. The direction is deliberate: over-reserving a typo
/// costs one request blocked slightly early plus an alertable warning, while
/// under-reserving a real model leaks spend past every cap silently.
pub fn reserve_request_cost(model: &str, input_tokens: u32, max_output_tokens: Option<u64>) -> f64 {
    estimate_request_cost(model, input_tokens, max_output_tokens).unwrap_or_else(|| {
        let p = assumed_unknown_price();
        warn_unknown_model(model, &canonical(model), p);
        let out = max_output_tokens.unwrap_or_else(assumed_output_tokens);
        (input_tokens as f64 / 1_000_000.0) * p.input_per_1m
            + (out as f64 / 1_000_000.0) * p.output_per_1m
    })
}

/// Billable request dimensions that are knowable before an upstream call.
///
/// A reservation must cover the most expensive outcome the request itself can
/// select. Anthropic cache writes are therefore treated as a miss and the
/// whole estimated prompt is assigned to the longest declared TTL. Explicit
/// breakpoints may cover less than the whole prompt, but reserving less would
/// let a strict cap be crossed on the first cache population. OpenAI's Chat
/// Completions web-search option represents one separately billed search call.
pub fn declared_request_usage(
    provider: &str,
    body: &Value,
    estimated_input_tokens: u32,
) -> BillableUsage {
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum CacheTtl {
        None,
        FiveMinutes,
        OneHour,
    }

    fn declared_cache_ttl(value: &Value) -> CacheTtl {
        match value {
            Value::Array(values) => values
                .iter()
                .map(declared_cache_ttl)
                .max()
                .unwrap_or(CacheTtl::None),
            Value::Object(object) => {
                let here = object
                    .get("cache_control")
                    .and_then(Value::as_object)
                    .filter(|cache| cache.get("type").and_then(Value::as_str) == Some("ephemeral"))
                    .map(|cache| match cache.get("ttl").and_then(Value::as_str) {
                        Some("1h") => CacheTtl::OneHour,
                        Some("5m") | None => CacheTtl::FiveMinutes,
                        Some(_) => CacheTtl::None,
                    })
                    .unwrap_or(CacheTtl::None);
                object
                    .values()
                    .map(declared_cache_ttl)
                    .fold(here, std::cmp::max)
            }
            _ => CacheTtl::None,
        }
    }

    let mut declared = BillableUsage::default();
    if provider.eq_ignore_ascii_case("anthropic") {
        // `inference_geo` may be supplied by the workspace rather than the
        // request. Reserve the 1.1x US-only rate unless the caller explicitly
        // pins global routing; settlement uses the provider-reported geo and
        // reconciles the conservative hold back to the exact amount.
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut multiplier = 1.0;
        if anthropic_supports_inference_geo(model)
            && body.get("inference_geo").and_then(Value::as_str) != Some("global")
        {
            multiplier *= 1.1;
        }
        if body.get("speed").and_then(Value::as_str) == Some("fast") {
            multiplier *= 2.0;
        }
        if multiplier > 1.0 {
            declared.token_price_multiplier = Some(multiplier);
        }
        match declared_cache_ttl(body) {
            CacheTtl::OneHour => declared.cache_write_1h_tokens = estimated_input_tokens,
            CacheTtl::FiveMinutes => declared.cache_write_tokens = estimated_input_tokens,
            CacheTtl::None => {}
        }
    } else if provider.eq_ignore_ascii_case("bedrock") {
        // The Bedrock Anthropic catalog rows are the global rates. Direct and
        // geography-scoped inference cost 10% more, including cache tokens.
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(pricing) = bedrock_anthropic_pricing(model) {
            if pricing.token_price_multiplier > 1.0 {
                declared.token_price_multiplier = Some(pricing.token_price_multiplier);
            }
        }
    }
    if provider.eq_ignore_ascii_case("openai")
        && body.get("web_search_options").is_some_and(Value::is_object)
    {
        declared
            .tool_calls
            .push(("openai:web_search".to_string(), 1));
    }
    declared
}

/// The forward reservation for a request, as a full breakdown.
///
/// Admission reserves against a request that has not run, so the only
/// dimensions it can see are the prompt it is about to send and the output
/// ceiling. It cannot know whether the provider will serve part of the prompt
/// from cache — which is *fine* for a reservation, because a cache hit is
/// always cheaper than the uncached input this reserves. Where it matters is
/// the cache-write and tool dimensions a caller can declare up front; those are
/// priced here so a fail-closed policy sees the same
/// [`CostBreakdown::blocks_fail_closed`] verdict before the call as after it.
pub fn reserve_request_breakdown(
    model: &str,
    input_tokens: u32,
    max_output_tokens: Option<u64>,
    declared: &BillableUsage,
) -> CostBreakdown {
    let declared_prompt_tokens = declared
        .cache_read_tokens
        .saturating_add(declared.cache_write_tokens)
        .saturating_add(declared.cache_write_1h_tokens);
    let usage = BillableUsage {
        // Declared cache dimensions are a split of the estimated prompt, not
        // extra tokens. `saturating_sub` also remains conservative when an
        // exact declaration is larger than our text-only token estimate.
        uncached_input_tokens: input_tokens.saturating_sub(declared_prompt_tokens),
        output_tokens: max_output_tokens
            .unwrap_or_else(assumed_output_tokens)
            .min(u32::MAX as u64) as u32,
        cache_read_tokens: declared.cache_read_tokens,
        cache_write_tokens: declared.cache_write_tokens,
        cache_write_1h_tokens: declared.cache_write_1h_tokens,
        tool_calls: declared.tool_calls.clone(),
        token_price_multiplier: declared.token_price_multiplier,
        billing_exempt: false,
        // A reservation is a forward estimate; there is no provider bill yet.
        provider_reported_cost_usd: None,
    };
    price_usage(model, &usage)
}

// ---------------------------------------------------------------------------
// Provider usage parsing
// ---------------------------------------------------------------------------

/// Whether this provider's prompt-token field already *includes* cached and
/// cache-written tokens.
///
/// This is the single most dangerous ambiguity in the whole cost model, and it
/// is not a matter of taste:
///
/// * OpenAI and the OpenAI-compatible providers report `prompt_tokens` as the
///   full prompt, with `cached_tokens` a *subset* of it. Charging both in full
///   would double-bill the cached prefix.
/// * Anthropic reports `input_tokens` as the prompt *excluding* cache reads and
///   cache creation, which are separate counters. AWS documents Bedrock the
///   same way: "the `inputTokens` field represents only the non-cached input
///   tokens". Subtracting there would erase real billable input.
///
/// The gateway's own Anthropic-to-OpenAI stream translation preserves
/// Anthropic's exclusive semantics (see `providers::anthropic_stream`), so the
/// answer follows the provider, not the body shape.
///
/// An unrecognized provider is treated as exclusive: that over-counts uncached
/// input rather than under-counting it, which is the safe direction.
fn prompt_tokens_include_cache(provider: &str) -> bool {
    matches!(
        provider.to_lowercase().as_str(),
        "openai"
            | "azure"
            | "azure_openai"
            | "openai_compatible"
            | "groq"
            | "together"
            | "fireworks"
            | "deepseek"
            | "xai"
            | "grok"
            | "mistral"
            | "perplexity"
            | "cohere"
    )
}

/// Whether a provider's response body carries a billed total the gateway may
/// trust in place of its own arithmetic.
pub fn is_authoritative_cost_provider(provider: &str) -> bool {
    catalog::AUTHORITATIVE_COST_PROVIDERS
        .iter()
        .any(|p| p.eq_ignore_ascii_case(provider))
}

fn u32_field(v: &Value, path: &[&str]) -> u32 {
    let mut cur = v;
    for key in path {
        match cur.get(key) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0).min(u32::MAX as u64) as u32
}

/// First non-zero value among several candidate paths.
fn first_u32(v: &Value, paths: &[&[&str]]) -> u32 {
    for path in paths {
        let found = u32_field(v, path);
        if found > 0 {
            return found;
        }
    }
    0
}

/// Extract the billable dimensions from a provider response body.
///
/// Returns `None` when the body carries no `usage` object at all — the caller
/// then falls back to its own token estimates rather than pretending the call
/// was free.
///
/// Documented fields read, per provider:
///
/// * **OpenAI** — `usage.prompt_tokens`, `usage.completion_tokens`,
///   `usage.prompt_tokens_details.cached_tokens` (also accepted under
///   `input_tokens_details`, the Responses API spelling), and
///   `cache_write_tokens`, which the GPT-5.6 family reports for tokens written
///   to the cache at 1.25x input.
/// * **Anthropic** — `usage.input_tokens`, `usage.output_tokens`,
///   `usage.cache_read_input_tokens`, `usage.cache_creation_input_tokens`, and
///   the per-TTL split `usage.cache_creation.ephemeral_5m_input_tokens` /
///   `ephemeral_1h_input_tokens` when present, since the 1-hour TTL bills at 2x
///   rather than 1.25x. Server tool use is read from
///   `usage.server_tool_use.web_search_requests` / `web_fetch_requests`.
/// * **Bedrock** — `cacheReadInputTokens` / `cacheWriteInputTokens` alongside
///   `inputTokens`.
/// * **Perplexity** — `usage.num_search_queries` priced against the tier named
///   by `usage.search_context_size`.
pub fn parse_usage(model: &str, provider: &str, body: &Value) -> Option<BillableUsage> {
    let usage = body.get("usage").or_else(|| {
        // Streamed responses are accumulated chunk by chunk; the terminal chunk
        // is sometimes handed here on its own.
        body.get("data").and_then(|d| d.get("usage"))
    })?;
    if !usage.is_object() {
        return None;
    }

    let mut out = BillableUsage::default();

    let cache_read = first_u32(
        usage,
        &[
            &["cache_read_input_tokens"],
            &["cacheReadInputTokens"],
            &["prompt_tokens_details", "cached_tokens"],
            &["input_tokens_details", "cached_tokens"],
        ],
    );
    // Anthropic's aggregate cache-creation counter equals the sum of the two
    // TTL buckets. Once either explicit bucket is present, both explicit
    // values (including zero) are authoritative; falling through from an
    // explicit 5m zero to the aggregate would count a pure 1h write twice.
    let has_ttl_breakdown = usage
        .get("cache_creation")
        .and_then(Value::as_object)
        .is_some_and(|creation| {
            creation.contains_key("ephemeral_5m_input_tokens")
                || creation.contains_key("ephemeral_1h_input_tokens")
        });
    let (cache_write_5m, cache_write_1h) = if has_ttl_breakdown {
        (
            u32_field(usage, &["cache_creation", "ephemeral_5m_input_tokens"]),
            u32_field(usage, &["cache_creation", "ephemeral_1h_input_tokens"]),
        )
    } else {
        (
            first_u32(
                usage,
                &[
                    &["cache_creation_input_tokens"],
                    &["cacheWriteInputTokens"],
                    &["prompt_tokens_details", "cache_write_tokens"],
                    &["input_tokens_details", "cache_write_tokens"],
                    &["cache_write_tokens"],
                ],
            ),
            0,
        )
    };

    let prompt = first_u32(
        usage,
        &[
            &["prompt_tokens"],
            &["input_tokens"],
            &["inputTokens"],
            &["promptTokens"],
        ],
    );
    out.output_tokens = first_u32(
        usage,
        &[
            &["completion_tokens"],
            &["output_tokens"],
            &["outputTokens"],
            &["completionTokens"],
        ],
    );
    if matches!(provider.to_ascii_lowercase().as_str(), "xai" | "grok") {
        // xAI's `total_tokens` includes billed hidden reasoning tokens that
        // some Chat responses omit from `completion_tokens`. Prefer the larger
        // derived output so a response without the exact-ticks field cannot
        // release a strict hold at visible-output cost only.
        let total = first_u32(usage, &[&["total_tokens"], &["totalTokens"]]);
        out.output_tokens = out.output_tokens.max(total.saturating_sub(prompt));
    }

    out.cache_read_tokens = cache_read;
    out.cache_write_tokens = cache_write_5m;
    out.cache_write_1h_tokens = cache_write_1h;
    out.uncached_input_tokens = if prompt_tokens_include_cache(provider) {
        prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write_5m)
            .saturating_sub(cache_write_1h)
    } else {
        prompt
    };
    if provider.eq_ignore_ascii_case("anthropic") {
        let mut multiplier = 1.0;
        if usage.get("inference_geo").and_then(Value::as_str) == Some("us") {
            multiplier *= 1.1;
        }
        if usage.get("speed").and_then(Value::as_str) == Some("fast") {
            multiplier *= 2.0;
        }
        if multiplier > 1.0 {
            out.token_price_multiplier = Some(multiplier);
        }
    } else if provider.eq_ignore_ascii_case("bedrock") {
        // Mirror the request hold at settlement so regional usage cannot
        // release a reservation at the cheaper global rate.
        if let Some(pricing) = bedrock_anthropic_pricing(model) {
            if pricing.token_price_multiplier > 1.0 {
                out.token_price_multiplier = Some(pricing.token_price_multiplier);
            }
        }
    }
    out.billing_exempt = provider.eq_ignore_ascii_case("anthropic")
        && out.output_tokens == 0
        && (body.get("stop_reason").and_then(Value::as_str) == Some("refusal")
            || usage
                .get("unbilled_refusal")
                .and_then(Value::as_bool)
                .unwrap_or(false));

    // Anthropic server tools. `web_fetch` is catalogued at $0, so counting it
    // is free but keeps the record honest about what ran.
    for (field, tool_id) in [
        ("web_search_requests", "anthropic:web_search"),
        ("web_fetch_requests", "anthropic:web_fetch"),
    ] {
        let calls = u32_field(usage, &["server_tool_use", field]);
        if calls > 0 {
            out.tool_calls.push((tool_id.to_string(), calls));
        }
    }

    // Perplexity bills a per-request search fee that scales with the requested
    // search context size, so the tier name is part of the tool id.
    let searches = first_u32(usage, &[&["num_search_queries"], &["numSearchQueries"]]);
    if searches > 0 {
        let size = usage
            .get("search_context_size")
            .and_then(Value::as_str)
            .unwrap_or("medium")
            .to_lowercase();
        out.tool_calls.push((
            format!("perplexity:{}:search_{size}", canonical(model)),
            searches,
        ));
    }

    if is_authoritative_cost_provider(provider) {
        out.provider_reported_cost_usd =
            if matches!(provider.to_ascii_lowercase().as_str(), "xai" | "grok") {
                usage
                    .get("cost_in_usd_ticks")
                    .and_then(Value::as_u64)
                    .map(|ticks| ticks as f64 / 10_000_000_000.0)
            } else {
                ["cost", "cost_usd", "total_cost_usd"]
                    .iter()
                    .find_map(|k| usage.get(k).and_then(Value::as_f64))
                    .filter(|c| c.is_finite() && *c >= 0.0)
            };
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A fixed instant, for tests that assert a rate scheduled to change.
    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

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
        assert!(lookup("gpt-4oxyz").is_none());
        assert_eq!(lookup("gpt-4o:free").unwrap().input_per_1m, 2.50);
        assert_eq!(lookup("gpt-4o-mini-2024-07-18").unwrap().input_per_1m, 0.15);
    }

    #[test]
    fn estimate_cost_math() {
        let c = estimate_cost("gpt-4o", 1_000_000, 1_000_000);
        assert!((c - 12.50).abs() < 1e-9);
    }

    #[test]
    fn estimate_cost_partial_tokens() {
        let c = estimate_cost("gpt-4o-mini", 1000, 500);
        let expected = (1000.0 / 1e6) * 0.15 + (500.0 / 1e6) * 0.60;
        assert!((c - expected).abs() < 1e-12);
    }

    /// NOV-135. Admission used to return `None` for an unknown model and every
    /// caller read that as $0, so an unknown id reserved nothing and was then
    /// billed at the catalog maximum. A cost cap could be walked past by naming
    /// a model the gateway had never heard of.
    #[test]
    fn an_unknown_model_reserves_the_assumption_not_zero() {
        let assumed = assumed_unknown_price();

        // Catalog-only signal still reports "no entry".
        assert!(estimate_request_cost("totally-made-up-model", 1000, Some(1000)).is_none());

        // The reservation path always yields, at the assumed rates.
        let reserved = reserve_request_cost("totally-made-up-model", 1000, Some(1000));
        let expected = (1000.0 / 1_000_000.0) * assumed.input_per_1m
            + (1000.0 / 1_000_000.0) * assumed.output_per_1m;
        assert!(
            (reserved - expected).abs() < f64::EPSILON,
            "expected the assumed rates, got {reserved}"
        );
        assert!(reserved > 0.0, "an unknown model must never reserve $0");

        // A known model is unaffected: it reserves exactly its catalog price.
        let known = reserve_request_cost("gpt-4o", 1000, Some(1000));
        assert_eq!(
            Some(known),
            estimate_request_cost("gpt-4o", 1000, Some(1000)),
            "a priced model must not be touched by the fallback"
        );
        assert!(
            known < reserved,
            "the assumption bounds every catalog rate, so it must cost more"
        );

        // Reserving and billing now rest on the SAME assumption, which is the
        // whole point: the cap sees what it will later be charged.
        let billed = price_call("totally-made-up-model", 1000, 1000);
        assert!(
            (billed.usd - reserved).abs() < f64::EPSILON,
            "reservation {reserved} and billing {} must agree",
            billed.usd
        );
    }

    #[test]
    fn unknown_model_is_never_free() {
        let a = assumed_unknown_price();
        let e = price_call("nope", 1000, 1000);
        assert!(e.is_assumed(), "unknown model must be tagged as assumed");
        assert_eq!(e.pricing, ModelPricing::UnknownAssumed(a));
        let expected = (1000.0 / 1e6) * a.input_per_1m + (1000.0 / 1e6) * a.output_per_1m;
        assert!((e.usd - expected).abs() < 1e-12);
        assert!(e.usd > 0.0, "unknown model must never cost $0");
        assert_eq!(price_call("nope", 0, 0).usd, 0.0);
        assert_eq!(
            price_call("gpt-4o", 1000, 500).usd,
            estimate_cost("gpt-4o", 1000, 500)
        );
        // `estimate_cost` stays catalog-only (0.0 = unpriced) for the provider
        // extractors; the middleware backfills the breakdown before the usage
        // event, which is where an unknown model stops being free.
        assert_eq!(estimate_cost("nope", 1000, 1000), 0.0);
    }

    #[test]
    fn assumed_rate_bounds_every_catalog_rate() {
        let a = assumed_unknown_price();
        for r in catalog::MODEL_ROWS {
            assert!(r.input_per_1m <= a.input_per_1m, "{} input", r.id);
            assert!(r.output_per_1m <= a.output_per_1m, "{} output", r.id);
        }
        for r in catalog::SCHEDULED_ROWS {
            assert!(r.input_per_1m <= a.input_per_1m, "{} scheduled input", r.id);
            assert!(
                r.output_per_1m <= a.output_per_1m,
                "{} scheduled output",
                r.id
            );
        }
        for r in catalog::LONG_CONTEXT_ROWS {
            assert!(r.input_per_1m <= a.input_per_1m, "{} tier input", r.id);
            assert!(r.output_per_1m <= a.output_per_1m, "{} tier output", r.id);
        }
        // Input is bounded by o1; output by GPT-5.6 Cyber.
        assert_eq!((a.input_per_1m, a.output_per_1m), (15.00, 75.00));
        let unknown = price_call("totally-made-up-model-xyz", 10_000, 10_000).usd;
        for r in catalog::MODEL_ROWS {
            assert!(
                price_call(r.id, 10_000, 10_000).usd <= unknown + 1e-12,
                "{} costs more than the unknown-model assumption",
                r.id
            );
        }
    }

    #[test]
    fn catalog_hits_are_not_priced_as_assumed() {
        for (model, expected_input) in [
            ("gpt-4o", 2.50),
            ("gpt-5.6", 4.00),
            ("gpt-4o-2024-11-20", 2.50),
            ("gpt-5.6-luna-2026-05-01", 0.20),
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
        assert!(price_for_context("gpt-4", 0).is_assumed());
        assert!(price_for_context("gpt-4oxyz", 0).is_assumed());
        assert_eq!(
            price_for_context("gpt-5.6-sol", 300_000)
                .price()
                .input_per_1m,
            8.00
        );
        assert_eq!(
            price_at("claude-sonnet-5", 0, at("2026-08-11T00:00:00Z")),
            ModelPricing::Catalog(ModelPrice {
                input_per_1m: 2.00,
                output_per_1m: 10.00
            })
        );
        // The increase to $3/$15 that was scheduled for this date WAS CANCELLED.
        // Anthropic's pricing page now states the introductory $2/$10 "is now
        // the standard price. The previously scheduled increase to $3/$15 per
        // million input/output tokens on September 1, 2026 will not occur."
        // Billing it would overcharge every Sonnet 5 call by 50%.
        // https://platform.claude.com/docs/en/about-claude/pricing
        assert_eq!(
            price_at("claude-sonnet-5", 0, at("2026-09-01T00:00:00Z")),
            ModelPricing::Catalog(ModelPrice {
                input_per_1m: 2.00,
                output_per_1m: 10.00
            })
        );
        // And it stays there well past the date, rather than merely being
        // deferred.
        assert_eq!(
            price_at("claude-sonnet-5", 0, at("2027-01-01T00:00:00Z")),
            ModelPricing::Catalog(ModelPrice {
                input_per_1m: 2.00,
                output_per_1m: 10.00
            })
        );
    }

    #[test]
    fn lookup_still_reports_unknown_models_as_unpriced() {
        assert!(lookup("totally-made-up-model-xyz").is_none());
        assert!(estimate_request_cost("totally-made-up-model-xyz", 10, Some(10)).is_none());
    }

    #[test]
    fn anthropic_and_bedrock_priced() {
        assert!(lookup("claude-opus-4-8").is_some());
        assert!(lookup("anthropic.claude-haiku-4-5-20251001-v1:0").is_some());
    }

    #[test]
    fn bedrock_anthropic_profile_ids_and_arns_resolve_to_the_global_catalog_row() {
        let direct = "anthropic.claude-sonnet-4-5-20250929-v1:0";
        let global = lookup(direct).expect("direct Bedrock model is catalogued");
        assert_eq!((global.input_per_1m, global.output_per_1m), (3.00, 15.00));

        for profile in [
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "au.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "jp.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "apac.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "arn:aws:bedrock:us-east-1:111122223333:inference-profile/global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "arn:aws:bedrock:eu-west-1:111122223333:inference-profile/eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ] {
            assert_eq!(lookup(profile), Some(global), "{profile}");
        }

        // Application profile ids are opaque and need a control-plane lookup;
        // a suggestive resource name is not enough to price one safely.
        assert!(lookup(
            "arn:aws:bedrock:us-east-1:111122223333:application-inference-profile/global.anthropic.claude-sonnet-4-5-20250929-v1:0"
        )
        .is_none());
        // Non-commercial partitions have separate regional price tables. Do
        // not project the commercial 1.1x rule onto them without a catalog row.
        assert!(lookup(
            "arn:aws-us-gov:bedrock:us-gov-west-1:111122223333:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        )
        .is_none());
        // Prefix normalization is deliberately Anthropic-only.
        assert!(lookup("global.amazon.nova-pro-v1:0").is_none());
        assert_eq!(lookup("amazon.nova-pro-v1:0").unwrap().input_per_1m, 0.8);
    }

    #[test]
    fn current_generation_models_priced() {
        let luna = lookup("gpt-5.6-luna").unwrap();
        assert_eq!((luna.input_per_1m, luna.output_per_1m), (0.20, 1.20));
        let terra = lookup("gpt-5.6-terra").unwrap();
        assert_eq!((terra.input_per_1m, terra.output_per_1m), (2.00, 12.00));
        let sol = lookup("gpt-5.6-sol").unwrap();
        assert_eq!((sol.input_per_1m, sol.output_per_1m), (4.00, 20.00));
        let sonnet5 = lookup_at("claude-sonnet-5", 0, at("2026-08-11T00:00:00Z")).unwrap();
        assert_eq!((sonnet5.input_per_1m, sonnet5.output_per_1m), (2.00, 10.00));
        for (model, expected) in [
            ("claude-opus-5", (5.00, 25.00, 0.50, 6.25, 10.00)),
            ("claude-opus-4-5-20251101", (5.00, 25.00, 0.50, 6.25, 10.00)),
            ("claude-mythos-5", (10.00, 50.00, 1.00, 12.50, 20.00)),
        ] {
            let row = rates_at(model, 0, at("2026-08-21T00:00:00Z"))
                .unwrap_or_else(|| panic!("missing active model {model}"));
            assert_eq!(
                (
                    row.input_per_1m,
                    row.output_per_1m,
                    row.cached_input_per_1m.unwrap(),
                    row.cache_write_per_1m.unwrap(),
                    row.cache_write_1h_per_1m.unwrap(),
                ),
                expected,
                "{model} rates"
            );
            assert!(
                !price_for_context(model, 0).is_assumed(),
                "{model} must not take the fail-closed unknown-model path"
            );
        }
        let flash = lookup("gemini-3.6-flash").unwrap();
        assert_eq!((flash.input_per_1m, flash.output_per_1m), (1.50, 7.50));
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
    fn gpt_5_6_standard_rates_match_official_pricing_page() {
        for (model, input, output) in [
            ("gpt-5.6-luna", 0.20, 1.20),
            ("gpt-5.6-terra", 2.00, 12.00),
            ("gpt-5.6-sol", 4.00, 20.00),
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
    fn gpt_5_6_sol_promotional_short_context_costs_and_reserves_sixty_cents() {
        for model in ["gpt-5.6-sol", "gpt-5.6", "daybreak-blue-latest"] {
            let cost = estimate_cost(model, 100_000, 10_000);
            let estimated = estimate_request_cost(model, 100_000, Some(10_000)).unwrap();
            let reserved = reserve_request_cost(model, 100_000, Some(10_000));

            assert!((cost - 0.60).abs() < 1e-12, "{model} cost {cost}");
            assert!(
                (estimated - 0.60).abs() < 1e-12,
                "{model} estimated {estimated}"
            );
            assert!(
                (reserved - 0.60).abs() < 1e-12,
                "{model} reserved {reserved}"
            );
            assert!((reserved - 0.80).abs() > 1e-12, "stale Sol reservation");
        }
    }

    #[test]
    fn gpt_5_6_sol_switches_token_and_cache_rates_only_above_272k() {
        let at_threshold = rates_at("gpt-5.6-sol", 272_000, at("2026-08-23T00:00:00Z"))
            .expect("Sol short-context rates");
        assert_eq!(
            (
                at_threshold.input_per_1m,
                at_threshold.cached_input_per_1m,
                at_threshold.cache_write_per_1m,
                at_threshold.output_per_1m,
            ),
            (4.00, Some(0.40), Some(5.00), 20.00)
        );

        let above_threshold = rates_at("gpt-5.6-sol", 272_001, at("2026-08-23T00:00:00Z"))
            .expect("Sol long-context rates");
        assert_eq!(
            (
                above_threshold.input_per_1m,
                above_threshold.cached_input_per_1m,
                above_threshold.cache_write_per_1m,
                above_threshold.output_per_1m,
            ),
            (8.00, Some(0.80), Some(10.00), 30.00)
        );
    }

    #[test]
    fn gpt_5_6_sol_aliases_keep_short_and_long_context_parity() {
        for input_tokens in [272_000, 272_001] {
            let canonical = rates_at("gpt-5.6-sol", input_tokens, at("2026-08-23T00:00:00Z"));
            for alias in ["gpt-5.6", "daybreak-blue-latest"] {
                assert_eq!(
                    rates_at(alias, input_tokens, at("2026-08-23T00:00:00Z")),
                    canonical,
                    "{alias} at {input_tokens} input tokens"
                );
            }
        }
    }

    #[test]
    fn gpt_5_6_cyber_uses_published_standard_rates() {
        let rates = rates_at("gpt-5.6-cyber", 0, at("2026-08-21T00:00:00Z"))
            .expect("the active Daybreak cyber model must be catalogued");
        assert_eq!(
            (
                rates.input_per_1m,
                rates.output_per_1m,
                rates.cached_input_per_1m,
                rates.cache_write_per_1m,
                rates.cache_write_1h_per_1m,
            ),
            (12.50, 75.00, Some(1.25), Some(15.625), None)
        );
    }

    #[test]
    fn unknown_model_output_assumption_covers_the_cyber_rate() {
        let assumed = assumed_unknown_price();
        assert!(
            assumed.output_per_1m >= 75.00,
            "an unknown model reserved ${}/M output, below the published cyber rate",
            assumed.output_per_1m
        );
    }

    #[test]
    fn daybreak_aliases_resolve_to_their_official_targets() {
        assert_eq!(
            lookup("daybreak-blue-latest"),
            lookup("gpt-5.6-sol"),
            "the blue alias must use the current Sol rate"
        );
        assert_eq!(
            lookup("daybreak-red-latest"),
            lookup("gpt-5.6-cyber"),
            "the red alias must use the current Cyber rate"
        );
    }

    #[test]
    fn gpt_5_6_snapshots_never_select_the_older_gpt_5_row() {
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
    fn sonnet_5_introductory_rate_became_the_standard_rate() {
        // This test used to assert the opposite, and asserting it was the bug:
        // it pinned a $3/$15 increase for 2026-09-01 that Anthropic has since
        // cancelled. "The $2/$10 ... pricing for Claude Sonnet 5, announced at
        // launch as introductory pricing through August 31, 2026, is now the
        // standard price. The previously scheduled increase to $3/$15 per
        // million input/output tokens on September 1, 2026 will not occur."
        // https://platform.claude.com/docs/en/about-claude/pricing
        //
        // Left uncorrected, every Sonnet 5 call would have been billed 50% high
        // from that date, with no deploy to make the change visible.
        let standard = (2.00, 10.00);
        for instant in [
            "2026-08-11T00:00:00Z",
            "2026-08-31T23:59:59Z",
            "2026-09-01T00:00:00Z",
            "2026-09-01T00:00:01Z",
            "2027-01-01T00:00:00Z",
        ] {
            let p = lookup_at("claude-sonnet-5", 0, at(instant)).unwrap();
            assert_eq!(
                (p.input_per_1m, p.output_per_1m),
                standard,
                "claude-sonnet-5 @ {instant}"
            );
            let snap = lookup_at("claude-sonnet-5-20260601", 0, at(instant)).unwrap();
            assert_eq!(snap, p, "snapshot @ {instant}");
        }
        // Nothing in the catalog moves this model's rate on that date.
        assert!(
            !catalog::SCHEDULED_ROWS
                .iter()
                .any(|r| r.id == "claude-sonnet-5"),
            "the cancelled Sonnet 5 increase must not be reinstated"
        );
        assert_eq!(
            lookup_at("gpt-4o", 0, at("2020-01-01T00:00:00Z")),
            lookup_at("gpt-4o", 0, at("2030-01-01T00:00:00Z"))
        );
    }

    #[test]
    fn scheduled_change_rescales_the_cache_dimensions() {
        // The scheduled table is empty today (the only entry it ever held, the
        // Sonnet 5 increase, was cancelled by Anthropic). The rescaling
        // machinery stays because the next repricing will need it, so it is
        // exercised here against a synthetic row rather than left to rot until
        // a real change lands and silently misbills cache reads.
        let row = catalog::MODEL_ROWS
            .iter()
            .find(|r| r.id == "claude-sonnet-5")
            .expect("claude-sonnet-5 is in the catalog");

        // Its published cache rates are the documented multiples of base input.
        assert_eq!(row.input_per_1m, 2.00);
        assert_eq!(row.cached_input_per_1m, Some(0.20));
        assert_eq!(row.cache_write_per_1m, Some(2.50));
        assert_eq!(row.cache_write_1h_per_1m, Some(4.00));

        let hypothetical = catalog::ScheduledRow {
            id: "claude-sonnet-5",
            effective_from_unix_secs: 1_788_220_800,
            input_per_1m: 3.00,
            output_per_1m: 15.00,
        };
        let rescaled = rescale_for_scheduled(row, &hypothetical);

        assert_eq!(rescaled.input_per_1m, 3.00);
        assert_eq!(rescaled.output_per_1m, 15.00);
        // 1.5x the base change, applied to every published multiple.
        assert!((rescaled.cached_input_per_1m.unwrap() - 0.30).abs() < 1e-12);
        assert!((rescaled.cache_write_per_1m.unwrap() - 3.75).abs() < 1e-12);
        assert!((rescaled.cache_write_1h_per_1m.unwrap() - 6.00).abs() < 1e-12);
        // The documented multipliers survive the rescale exactly.
        assert!(
            (rescaled.cached_input_per_1m.unwrap() / rescaled.input_per_1m - 0.1).abs() < 1e-12
        );
        assert!(
            (rescaled.cache_write_1h_per_1m.unwrap() / rescaled.input_per_1m - 2.0).abs() < 1e-12
        );
    }

    #[test]
    fn no_price_increase_is_scheduled_for_any_model() {
        // A guard, not a preference. A scheduled row silently changes what
        // every customer is billed on a future date with no deploy, so one
        // must never appear without a source link reviewed at the time. If
        // this fails, a rate was added: verify it against the provider's
        // published page and update the boundary tests deliberately.
        assert!(
            catalog::SCHEDULED_ROWS.is_empty(),
            "a scheduled price change was added: {:?}",
            catalog::SCHEDULED_ROWS
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn scheduled_models_have_no_long_context_tier() {
        // Rates return a long-context tier before consulting the scheduled
        // table, so a scheduled model that also carried a tier row would
        // silently ignore its own price change above the threshold. No such
        // model exists today; if one appears, make the tier table date-aware
        // rather than deleting this test.
        for sched in catalog::SCHEDULED_ROWS {
            assert!(
                !catalog::LONG_CONTEXT_ROWS
                    .iter()
                    .any(|tier| tier.id == sched.id),
                "{} has both a scheduled price change and a long-context tier",
                sched.id
            );
        }
    }

    #[test]
    fn bare_gpt_5_6_alias_resolves_to_sol() {
        let alias = lookup("gpt-5.6").unwrap();
        assert_eq!((alias.input_per_1m, alias.output_per_1m), (4.00, 20.00));
        assert_eq!(alias, lookup("gpt-5.6-sol").unwrap());
        assert_ne!(alias, lookup("gpt-5").unwrap());
        assert_eq!(lookup("GPT-5.6").unwrap(), alias);
        let r = estimate_request_cost("gpt-5.6", 6, Some(8)).unwrap();
        let expected = (6.0 / 1e6) * 4.00 + (8.0 / 1e6) * 20.00;
        assert!(
            (r - expected).abs() < 1e-12,
            "alias reserved {r} vs {expected}"
        );
        let stale = estimate_request_cost("gpt-5", 6, Some(8)).unwrap();
        assert!(r > stale * 2.0);
        let hi = lookup_for_context("gpt-5.6", 300_000).unwrap();
        assert_eq!((hi.input_per_1m, hi.output_per_1m), (8.00, 30.00));
    }

    #[test]
    fn long_context_tier_applies_to_whole_gpt_5_6_family() {
        for (model, std_rates, hi_rates) in [
            ("gpt-5.6-luna", (0.20, 1.20), (0.40, 1.80)),
            ("gpt-5.6-terra", (2.00, 12.00), (4.00, 18.00)),
            ("gpt-5.6-sol", (4.00, 20.00), (8.00, 30.00)),
        ] {
            let at_threshold = lookup_for_context(model, 272_000).unwrap();
            assert_eq!(
                (at_threshold.input_per_1m, at_threshold.output_per_1m),
                std_rates,
                "{model} @272K"
            );
            let over = lookup_for_context(model, 272_001).unwrap();
            assert_eq!(
                (over.input_per_1m, over.output_per_1m),
                hi_rates,
                "{model} >272K"
            );
            assert!((hi_rates.0 - std_rates.0 * 2.0).abs() < 1e-9);
            assert!((hi_rates.1 - std_rates.1 * 1.5).abs() < 1e-9);
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
        let base = lookup_for_context("gpt-5.6-luna", 272_000).unwrap();
        assert_eq!((base.input_per_1m, base.output_per_1m), (0.20, 1.20));
        let hi = lookup_for_context("gpt-5.6-luna", 272_001).unwrap();
        assert_eq!((hi.input_per_1m, hi.output_per_1m), (0.40, 1.80));
        let hi_snap = lookup_for_context("gpt-5.6-luna-2026-05-01", 300_000).unwrap();
        assert_eq!(hi_snap.input_per_1m, 0.40);
        let g = lookup_for_context("gemini-2.5-pro", 250_000).unwrap();
        assert_eq!((g.input_per_1m, g.output_per_1m), (2.50, 15.00));
        let flat = lookup_for_context("gpt-4o", 5_000_000).unwrap();
        assert_eq!(flat.input_per_1m, 2.50);
        let c = estimate_cost("gpt-5.6-luna", 300_000, 1000);
        let expected = (300_000.0 / 1e6) * 0.40 + (1000.0 / 1e6) * 1.80;
        assert!((c - expected).abs() < 1e-9);
        let r = estimate_request_cost("gpt-5.6-luna", 300_000, Some(1000)).unwrap();
        assert!((r - expected).abs() < 1e-9);
    }

    #[test]
    fn long_context_estimates_at_300k_match_published_rates() {
        for (model, expected) in [
            ("gpt-5.6-luna", 0.1218),
            ("gpt-5.6-terra", 1.2180),
            ("gpt-5.6-sol", 2.4300),
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
        let c = estimate_request_cost("gpt-4o", 10, Some(1000)).unwrap();
        let expected = (10.0 / 1e6) * 2.50 + (1000.0 / 1e6) * 10.00;
        assert!((c - expected).abs() < 1e-12);
        let d = estimate_request_cost("gpt-4o", 10, None).unwrap();
        let expected_default =
            (10.0 / 1e6) * 2.50 + (DEFAULT_ASSUMED_OUTPUT_TOKENS as f64 / 1e6) * 10.00;
        assert!((d - expected_default).abs() < 1e-12);
        assert!(estimate_request_cost("no-such-model", 10, Some(10)).is_none());
    }

    // -- catalog integrity ------------------------------------------------

    #[test]
    fn rust_pricing_tables_match_the_committed_catalog() {
        const RATE_EPSILON: f64 = 1e-9;

        fn field<'a>(entry: &'a Value, id: &str, key: &str) -> &'a Value {
            entry
                .get(key)
                .unwrap_or_else(|| panic!("{id}: catalog entry has no field `{key}`"))
        }

        fn required_rate(entry: &Value, id: &str, key: &str) -> f64 {
            field(entry, id, key)
                .as_f64()
                .unwrap_or_else(|| panic!("{id}: catalog field `{key}` is not a number"))
        }

        fn optional_rate(entry: &Value, id: &str, key: &str) -> Option<f64> {
            let value = field(entry, id, key);
            if value.is_null() {
                return None;
            }
            Some(
                value.as_f64().unwrap_or_else(|| {
                    panic!("{id}: catalog field `{key}` is not a number or null")
                }),
            )
        }

        fn assert_rate_eq(id: &str, key: &str, from_catalog: f64, from_rust: f64) {
            assert!(
                (from_catalog - from_rust).abs() < RATE_EPSILON,
                "{id}: field `{key}` disagrees — pricing/catalog.json has {from_catalog}, \
                 the Rust table has {from_rust}"
            );
        }

        fn assert_optional_rate_eq(
            id: &str,
            key: &str,
            from_catalog: Option<f64>,
            from_rust: Option<f64>,
        ) {
            match (from_catalog, from_rust) {
                (None, None) => {}
                (Some(c), Some(r)) => assert_rate_eq(id, key, c, r),
                (c, r) => panic!(
                    "{id}: field `{key}` disagrees on whether the dimension is published — \
                     pricing/catalog.json has {c:?}, the Rust table has {r:?}; a missing \
                     dimension is not the same as a zero rate"
                ),
            }
        }

        let catalog_json = include_str!("../../pricing/catalog.json");
        let parsed: Value = serde_json::from_str(catalog_json).unwrap();
        assert_eq!(parsed["version"].as_str().unwrap(), CATALOG_VERSION);

        let rust_entries: Vec<&Value> = parsed["models"]
            .as_array()
            .expect("catalog `models` is not an array")
            .iter()
            .filter(|m| {
                m["targets"]
                    .as_array()
                    .expect("catalog model entry has no `targets` array")
                    .iter()
                    .any(|t| t == "rust")
            })
            .collect();

        for entry in &rust_entries {
            let id = entry["id"]
                .as_str()
                .expect("catalog model entry has no `id`");
            let row = catalog::MODEL_ROWS
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| {
                    panic!(
                        "{id}: pricing/catalog.json targets `rust` but MODEL_ROWS has no row \
                         with that id"
                    )
                });

            let provider = field(entry, id, "provider")
                .as_str()
                .unwrap_or_else(|| panic!("{id}: catalog field `provider` is not a string"));
            assert_eq!(
                provider, row.provider,
                "{id}: field `provider` disagrees — pricing/catalog.json has {provider}, \
                 the Rust table has {}",
                row.provider
            );

            assert_rate_eq(
                id,
                "inputPer1m",
                required_rate(entry, id, "inputPer1m"),
                row.input_per_1m,
            );
            assert_rate_eq(
                id,
                "outputPer1m",
                required_rate(entry, id, "outputPer1m"),
                row.output_per_1m,
            );
            assert_optional_rate_eq(
                id,
                "cachedInputPer1m",
                optional_rate(entry, id, "cachedInputPer1m"),
                row.cached_input_per_1m,
            );
            assert_optional_rate_eq(
                id,
                "cacheWritePer1m",
                optional_rate(entry, id, "cacheWritePer1m"),
                row.cache_write_per_1m,
            );
            assert_optional_rate_eq(
                id,
                "cacheWrite1hPer1m",
                optional_rate(entry, id, "cacheWrite1hPer1m"),
                row.cache_write_1h_per_1m,
            );
        }

        for row in catalog::MODEL_ROWS {
            let matches = rust_entries
                .iter()
                .filter(|e| e["id"].as_str() == Some(row.id))
                .count();
            assert_eq!(
                matches, 1,
                "{}: MODEL_ROWS carries this model but pricing/catalog.json has {matches} \
                 `rust`-targeted entries for it",
                row.id
            );
        }
        assert_eq!(
            rust_entries.len(),
            catalog::MODEL_ROWS.len(),
            "pricing/catalog.json has {} `rust`-targeted entries but MODEL_ROWS has {} rows",
            rust_entries.len(),
            catalog::MODEL_ROWS.len()
        );

        let long_context_entries = parsed["longContext"]
            .as_array()
            .expect("catalog `longContext` is not an array");

        for entry in long_context_entries {
            let id = entry["id"]
                .as_str()
                .expect("catalog longContext entry has no `id`");
            let row = catalog::LONG_CONTEXT_ROWS
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| {
                    panic!(
                        "{id}: pricing/catalog.json declares a long-context tier but \
                         LONG_CONTEXT_ROWS has no row with that id"
                    )
                });

            let threshold = field(entry, id, "thresholdInputTokens")
                .as_u64()
                .unwrap_or_else(|| {
                    panic!("{id}: catalog field `thresholdInputTokens` is not an unsigned integer")
                });
            assert_eq!(
                threshold,
                u64::from(row.threshold_input_tokens),
                "{id}: field `thresholdInputTokens` disagrees — pricing/catalog.json has \
                 {threshold}, the Rust table has {}",
                row.threshold_input_tokens
            );

            assert_rate_eq(
                id,
                "longContext.inputPer1m",
                required_rate(entry, id, "inputPer1m"),
                row.input_per_1m,
            );
            assert_rate_eq(
                id,
                "longContext.outputPer1m",
                required_rate(entry, id, "outputPer1m"),
                row.output_per_1m,
            );
            assert_optional_rate_eq(
                id,
                "longContext.cachedInputPer1m",
                optional_rate(entry, id, "cachedInputPer1m"),
                row.cached_input_per_1m,
            );
            assert_optional_rate_eq(
                id,
                "longContext.cacheWritePer1m",
                optional_rate(entry, id, "cacheWritePer1m"),
                row.cache_write_per_1m,
            );
        }

        for row in catalog::LONG_CONTEXT_ROWS {
            let matches = long_context_entries
                .iter()
                .filter(|e| e["id"].as_str() == Some(row.id))
                .count();
            assert_eq!(
                matches, 1,
                "{}: LONG_CONTEXT_ROWS carries this tier but pricing/catalog.json has {matches} \
                 `longContext` entries for it",
                row.id
            );
        }
        assert_eq!(
            long_context_entries.len(),
            catalog::LONG_CONTEXT_ROWS.len(),
            "pricing/catalog.json has {} `longContext` entries but LONG_CONTEXT_ROWS has {} rows",
            long_context_entries.len(),
            catalog::LONG_CONTEXT_ROWS.len()
        );

        let scheduled_entries = parsed["scheduled"]
            .as_array()
            .expect("catalog `scheduled` is not an array");
        for entry in scheduled_entries {
            let id = entry["id"]
                .as_str()
                .expect("catalog scheduled entry has no `id`");
            let row = catalog::SCHEDULED_ROWS
                .iter()
                .find(|row| row.id == id)
                .unwrap_or_else(|| {
                    panic!(
                        "{id}: pricing/catalog.json declares a scheduled rate but \
                         SCHEDULED_ROWS has no row with that id"
                    )
                });
            let effective = field(entry, id, "effectiveFromUnixSecs")
                .as_i64()
                .unwrap_or_else(|| {
                    panic!("{id}: catalog field `effectiveFromUnixSecs` is not a signed integer")
                });
            assert_eq!(
                effective, row.effective_from_unix_secs,
                "{id}: field `effectiveFromUnixSecs` disagrees"
            );
            assert_rate_eq(
                id,
                "scheduled.inputPer1m",
                required_rate(entry, id, "inputPer1m"),
                row.input_per_1m,
            );
            assert_rate_eq(
                id,
                "scheduled.outputPer1m",
                required_rate(entry, id, "outputPer1m"),
                row.output_per_1m,
            );
        }
        for row in catalog::SCHEDULED_ROWS {
            let matches = scheduled_entries
                .iter()
                .filter(|entry| entry["id"].as_str() == Some(row.id))
                .count();
            assert_eq!(
                matches, 1,
                "{}: SCHEDULED_ROWS carries this change but pricing/catalog.json has \
                 {matches} scheduled entries for it",
                row.id
            );
        }
        assert_eq!(
            scheduled_entries.len(),
            catalog::SCHEDULED_ROWS.len(),
            "pricing/catalog.json has {} scheduled entries but SCHEDULED_ROWS has {} rows",
            scheduled_entries.len(),
            catalog::SCHEDULED_ROWS.len()
        );

        let alias_entries = parsed["aliases"]
            .as_array()
            .expect("catalog `aliases` is not an array");
        for entry in alias_entries {
            let alias = entry["alias"]
                .as_str()
                .expect("catalog alias entry has no string `alias`");
            let target = entry["target"]
                .as_str()
                .expect("catalog alias entry has no string `target`");
            assert!(
                catalog::MODEL_ALIASES
                    .iter()
                    .any(|(rust_alias, rust_target)|
                        *rust_alias == alias && *rust_target == target),
                "pricing/catalog.json maps {alias} to {target}, but MODEL_ALIASES does not"
            );
        }
        for (alias, target) in catalog::MODEL_ALIASES {
            let matches = alias_entries
                .iter()
                .filter(|entry| {
                    entry["alias"].as_str() == Some(alias)
                        && entry["target"].as_str() == Some(target)
                })
                .count();
            assert_eq!(
                matches, 1,
                "MODEL_ALIASES maps {alias} to {target}, but pricing/catalog.json has \
                 {matches} matching entries"
            );
        }
        assert_eq!(
            alias_entries.len(),
            catalog::MODEL_ALIASES.len(),
            "pricing/catalog.json has {} aliases but MODEL_ALIASES has {} entries",
            alias_entries.len(),
            catalog::MODEL_ALIASES.len()
        );

        let tool_fee_entries = parsed["toolFees"]
            .as_array()
            .expect("catalog `toolFees` is not an array");
        for entry in tool_fee_entries {
            let id = entry["id"]
                .as_str()
                .expect("catalog tool fee entry has no string `id`");
            let (_, rust_fee) = catalog::TOOL_FEES_USD_PER_1K_CALLS
                .iter()
                .find(|(rust_id, _)| *rust_id == id)
                .unwrap_or_else(|| {
                    panic!(
                        "{id}: pricing/catalog.json declares a tool fee but \
                         TOOL_FEES_USD_PER_1K_CALLS has no entry"
                    )
                });
            assert_rate_eq(
                id,
                "toolFees.usdPer1kCalls",
                required_rate(entry, id, "usdPer1kCalls"),
                *rust_fee,
            );
        }
        for (id, _) in catalog::TOOL_FEES_USD_PER_1K_CALLS {
            let matches = tool_fee_entries
                .iter()
                .filter(|entry| entry["id"].as_str() == Some(id))
                .count();
            assert_eq!(
                matches, 1,
                "TOOL_FEES_USD_PER_1K_CALLS carries {id}, but pricing/catalog.json has \
                 {matches} tool fee entries for it"
            );
        }
        assert_eq!(
            tool_fee_entries.len(),
            catalog::TOOL_FEES_USD_PER_1K_CALLS.len(),
            "pricing/catalog.json has {} tool fees but TOOL_FEES_USD_PER_1K_CALLS has {} entries",
            tool_fee_entries.len(),
            catalog::TOOL_FEES_USD_PER_1K_CALLS.len()
        );
    }

    #[test]
    fn catalog_never_confuses_free_with_unpublished() {
        // `Some(0.0)` means documented-free, `None` means unpublished. A row
        // that publishes a cached rate but no write rate (or vice versa) is
        // fine; what must never happen is a rate that is negative or NaN.
        for r in catalog::MODEL_ROWS {
            assert!(
                r.input_per_1m >= 0.0 && r.input_per_1m.is_finite(),
                "{}",
                r.id
            );
            assert!(
                r.output_per_1m >= 0.0 && r.output_per_1m.is_finite(),
                "{}",
                r.id
            );
            for rate in [
                r.cached_input_per_1m,
                r.cache_write_per_1m,
                r.cache_write_1h_per_1m,
            ]
            .into_iter()
            .flatten()
            {
                assert!(rate >= 0.0 && rate.is_finite(), "{}", r.id);
            }
            // A published cache read is always a discount on input; a published
            // write is always a premium. A row that violates either is a
            // transcription error, and both directions bias billing.
            if let Some(cached) = r.cached_input_per_1m {
                assert!(
                    cached <= r.input_per_1m + 1e-12,
                    "{} cache read {cached} exceeds input {}",
                    r.id,
                    r.input_per_1m
                );
            }
            for write in [r.cache_write_per_1m, r.cache_write_1h_per_1m]
                .into_iter()
                .flatten()
                .filter(|w| *w > 0.0)
            {
                assert!(
                    write >= r.input_per_1m - 1e-12,
                    "{} cache write {write} is below input {}",
                    r.id,
                    r.input_per_1m
                );
            }
        }
        assert_eq!(max_cache_write_multiplier(), 2.0, "Anthropic's 1-hour TTL");
        assert_eq!(max_tool_fee_per_1k_calls(), 35.0, "Gemini 2.5 grounding");
    }

    #[test]
    fn published_cache_rates_match_their_sources() {
        // Absolute figures read off the official pages, so a transcription slip
        // in the catalog fails here rather than in someone's invoice.
        //
        // OpenAI: developers.openai.com/api/docs/pricing plus the prompt-caching
        // guide ("For GPT-5.6 models and later model families, cache writes cost
        // 1.25x the uncached input token rate"; earlier families have no write
        // fee). Cached-input multipliers are NOT uniform: 0.10x on the 5.x
        // families, 0.25x on 4.1/o3/o4-mini, 0.50x on 4o.
        for (model, cached, write) in [
            ("gpt-5.6-luna", Some(0.02), Some(0.25)),
            ("gpt-5.6-terra", Some(0.20), Some(2.50)),
            ("gpt-5.6-sol", Some(0.40), Some(5.00)),
            ("gpt-5", Some(0.125), Some(0.0)),
            ("gpt-4.1", Some(0.50), Some(0.0)),
            ("gpt-4o", Some(1.25), Some(0.0)),
            ("o3", Some(0.50), Some(0.0)),
            ("o4-mini", Some(0.275), Some(0.0)),
            // o1 predates the published multipliers and the current pricing
            // page lists no cached rate for it: unknown, not guessed.
            ("o1", None, Some(0.0)),
        ] {
            let r = rates_at(model, 0, at("2026-08-12T00:00:00Z")).unwrap();
            assert_eq!(r.cached_input_per_1m, cached, "{model} cached input");
            assert_eq!(r.cache_write_per_1m, write, "{model} cache write");
        }

        // Anthropic: platform.claude.com/docs/en/about-claude/pricing publishes
        // cache read at 0.1x base, a 5-minute write at 1.25x and a 1-hour write
        // at 2x.
        for (model, base) in [
            ("claude-sonnet-4-5", 3.00),
            ("claude-opus-4-8", 5.00),
            ("claude-haiku-4-5", 1.00),
            ("claude-fable-5", 10.00),
        ] {
            let r = rates_at(model, 0, at("2026-08-12T00:00:00Z")).unwrap();
            assert_eq!(r.input_per_1m, base, "{model} base");
            let close = |got: Option<f64>, want: f64| (got.expect("published") - want).abs() < 1e-9;
            assert!(close(r.cached_input_per_1m, base * 0.1), "{model} read");
            assert!(close(r.cache_write_per_1m, base * 1.25), "{model} 5m write");
            assert!(
                close(r.cache_write_1h_per_1m, base * 2.0),
                "{model} 1h write"
            );
        }

        // Google: ai.google.dev/gemini-api/docs/pricing prices context caching
        // at 0.1x input and charges cached content as storage per hour rather
        // than per written token, so the write dimension is documented-free.
        let g = rates_at("gemini-3.6-flash", 0, at("2026-08-12T00:00:00Z")).unwrap();
        assert_eq!(g.cached_input_per_1m, Some(0.15));
        assert_eq!(g.cache_write_per_1m, Some(0.0));

        // DeepSeek: api-docs.deepseek.com publishes an absolute cache-hit rate
        // per model, and the two models do NOT share a multiplier.
        let flash = rates_at("deepseek-v4-flash", 0, at("2026-08-12T00:00:00Z")).unwrap();
        assert_eq!(flash.cached_input_per_1m, Some(0.0028));
        let pro = rates_at("deepseek-v4-pro", 0, at("2026-08-12T00:00:00Z")).unwrap();
        assert_eq!(pro.cached_input_per_1m, Some(0.003625));
        assert_ne!(
            flash.cached_input_per_1m.unwrap() / flash.input_per_1m,
            pro.cached_input_per_1m.unwrap() / pro.input_per_1m,
        );

        // xAI: docs.x.ai/docs/models, cached input on grok-4.3, doubling above
        // a 200K prompt.
        let grok = rates_at("grok-4.3", 0, at("2026-08-12T00:00:00Z")).unwrap();
        assert_eq!(grok.cached_input_per_1m, Some(0.20));
        let grok_long = rates_at("grok-4.3", 250_000, at("2026-08-12T00:00:00Z")).unwrap();
        assert_eq!(grok_long.input_per_1m, 2.50);
        assert_eq!(grok_long.cached_input_per_1m, Some(0.40));
    }

    // -- the breakdown ----------------------------------------------------

    #[test]
    fn cached_input_is_billed_at_the_cached_rate_not_the_input_rate() {
        // 100K prompt tokens of which 90K were a cache hit, plus 1K output.
        // Anthropic Sonnet 4.5: $3 input, $0.30 cache read, $15 output.
        let usage = BillableUsage {
            uncached_input_tokens: 10_000,
            cache_read_tokens: 90_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        let b = price_usage("claude-sonnet-4-5", &usage);
        assert!(b.is_complete);
        assert!(b.missing_dimensions.is_empty());
        assert_eq!(b.source, CostSource::Catalog);
        assert!((b.uncached_input_usd - 0.03).abs() < 1e-12);
        assert!((b.cache_read_usd - 0.027).abs() < 1e-12);
        assert!((b.output_usd - 0.015).abs() < 1e-12);
        assert_eq!(b.cache_write_usd, 0.0);
        assert!((b.total_usd - 0.072).abs() < 1e-12);
        // The whole point: pricing the cached prefix at the input rate would
        // charge 0.27 for it, ten times too much.
        let naive = (100_000.0 / 1e6) * 3.00 + (1_000.0 / 1e6) * 15.00;
        assert!(b.total_usd < naive * 0.30, "cached read must be discounted");
        // Components always reconstruct the total.
        assert!(
            (b.uncached_input_usd
                + b.cache_read_usd
                + b.cache_write_usd
                + b.output_usd
                + b.tool_usd
                - b.total_usd)
                .abs()
                < 1e-12
        );
        assert_eq!(b.pricing_version, CATALOG_VERSION);
    }

    #[test]
    fn cache_writes_are_billed_at_a_premium_and_split_by_ttl() {
        // Anthropic Opus 4.8: $5 base, $6.25 five-minute write, $10 one-hour.
        let usage = BillableUsage {
            uncached_input_tokens: 1_000,
            cache_write_tokens: 100_000,
            cache_write_1h_tokens: 50_000,
            output_tokens: 100,
            ..Default::default()
        };
        let b = price_usage("claude-opus-4-8", &usage);
        assert!(b.is_complete, "both TTLs are published");
        let expected_write = (100_000.0 / 1e6) * 6.25 + (50_000.0 / 1e6) * 10.00;
        assert!((b.cache_write_usd - expected_write).abs() < 1e-12);
        // A write costs strictly more than the same tokens as plain input.
        let as_input = (150_000.0 / 1e6) * 5.00;
        assert!(b.cache_write_usd > as_input);
        // GPT-5.6 charges 1.25x; earlier OpenAI families charge nothing.
        // Kept under the 272K tier threshold so this exercises the published
        // short-context write rate rather than the tier.
        let sol = price_usage(
            "gpt-5.6-sol",
            &BillableUsage {
                cache_write_tokens: 100_000,
                ..Default::default()
            },
        );
        assert!((sol.cache_write_usd - 0.50).abs() < 1e-12);
        assert!(sol.is_complete);
        let four_o = price_usage(
            "gpt-4o",
            &BillableUsage {
                cache_write_tokens: 1_000_000,
                ..Default::default()
            },
        );
        assert_eq!(four_o.cache_write_usd, 0.0);
        assert!(
            four_o.is_complete,
            "documented-free is a real zero, not a missing dimension"
        );
    }

    #[test]
    fn tool_and_search_fees_are_billed_per_request() {
        // Anthropic web search: $10 per 1,000 searches.
        let usage = BillableUsage {
            uncached_input_tokens: 1_000,
            output_tokens: 500,
            tool_calls: vec![
                ("anthropic:web_search".to_string(), 3),
                ("anthropic:web_fetch".to_string(), 5),
            ],
            ..Default::default()
        };
        let b = price_usage("claude-sonnet-4-5", &usage);
        assert!(b.is_complete);
        assert!((b.tool_usd - 0.03).abs() < 1e-12, "3 searches at $10/1k");
        // Web fetch is documented as free and must not inflate the bill.
        assert!(b.total_usd > b.tool_usd);

        // Perplexity's fee scales with the search context tier.
        let pplx = price_usage(
            "sonar-pro",
            &BillableUsage {
                uncached_input_tokens: 100,
                output_tokens: 100,
                tool_calls: vec![("perplexity:sonar-pro:search_high".to_string(), 1)],
                ..Default::default()
            },
        );
        assert!((pplx.tool_usd - 0.014).abs() < 1e-12);
        assert!(pplx.is_complete);
    }

    #[test]
    fn an_unknown_tool_fee_is_charged_conservatively_and_flagged() {
        let b = price_usage(
            "gpt-4o",
            &BillableUsage {
                uncached_input_tokens: 100,
                output_tokens: 100,
                tool_calls: vec![("openai:some_new_tool".to_string(), 2)],
                ..Default::default()
            },
        );
        assert!(!b.is_complete);
        assert_eq!(b.missing_dimensions, vec![BillableDimension::Tool]);
        // Charged at the largest fee the catalog knows, not at zero.
        assert!((b.tool_usd - (2.0 / 1000.0) * 35.0).abs() < 1e-12);
        assert!(b.tool_usd > 0.0);
    }

    #[test]
    fn a_provider_reported_total_wins_when_it_is_authoritative() {
        // Generic provider cost fields remain untrusted: an unverified number
        // must not become the bill.
        let body = json!({
            "usage": { "prompt_tokens": 1000, "completion_tokens": 1000, "cost": 42.0 }
        });
        let parsed = parse_usage("gpt-4o", "openai", &body).unwrap();
        assert_eq!(parsed.provider_reported_cost_usd, None);
        let priced = price_usage("gpt-4o", &parsed);
        assert_eq!(priced.source, CostSource::Catalog);
        assert!(priced.total_usd < 1.0);

        // xAI documents cost_in_usd_ticks as the exact billed total, inclusive
        // of caching, discounts, and server-side tools. One USD is 1e10 ticks.
        let xai_body = json!({
            "usage": {
                "prompt_tokens": 199,
                "completion_tokens": 1,
                "cost_in_usd_ticks": 37_756_000_u64
            }
        });
        for provider in ["xai", "grok"] {
            let parsed = parse_usage("grok-4.6", provider, &xai_body).unwrap();
            assert_eq!(parsed.provider_reported_cost_usd, Some(0.0037756));
            let priced = price_usage("grok-4.6", &parsed);
            assert_eq!(priced.source, CostSource::ProviderReported);
            assert_eq!(priced.total_usd, 0.0037756);
        }

        // Some valid xAI responses omit ticks. `total_tokens` still includes
        // hidden reasoning tokens, so the output charged by the catalog must
        // not stop at the visible completion-token count.
        let xai_without_ticks = json!({
            "usage": {
                "prompt_tokens": 32,
                "completion_tokens": 9,
                "total_tokens": 135,
                "completion_tokens_details": {"reasoning_tokens": 94}
            }
        });
        let parsed = parse_usage("grok-4.6", "xai", &xai_without_ticks).unwrap();
        assert_eq!(parsed.uncached_input_tokens, 32);
        assert_eq!(parsed.output_tokens, 103);
        assert_eq!(parsed.provider_reported_cost_usd, None);

        // When a total IS authoritative it supersedes the arithmetic outright,
        // and the components remain for audit.
        let usage = BillableUsage {
            uncached_input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_tokens: 5_000,
            provider_reported_cost_usd: Some(0.5),
            ..Default::default()
        };
        let b = price_usage("gpt-4o", &usage);
        assert_eq!(b.source, CostSource::ProviderReported);
        assert_eq!(b.total_usd, 0.5);
        assert!(b.is_complete);
        assert!(b.uncached_input_usd > 0.0, "components stay for audit");
        assert!(!b.blocks_fail_closed());

        // An authoritative total also settles a dimension we could not price:
        // the provider billed us, so nothing is missing.
        let unpriceable = BillableUsage {
            uncached_input_tokens: 1_000,
            cache_read_tokens: 1_000,
            provider_reported_cost_usd: Some(0.25),
            ..Default::default()
        };
        let b = price_usage("o1", &unpriceable);
        assert_eq!(b.total_usd, 0.25);
        assert!(b.is_complete);
        assert!(b.missing_dimensions.is_empty());
        // A nonsense total is ignored rather than trusted.
        let nan = BillableUsage {
            uncached_input_tokens: 1_000,
            provider_reported_cost_usd: Some(f64::NAN),
            ..Default::default()
        };
        let b = price_usage("gpt-4o", &nan);
        assert_eq!(b.source, CostSource::Catalog);
        assert!(b.total_usd > 0.0);
    }

    #[test]
    fn a_missing_dimension_never_posts_a_silent_zero() {
        // o1 reports cached tokens but publishes no cached rate. The tokens are
        // real spend; charging $0 for them is the bug this whole type prevents.
        let usage = BillableUsage {
            uncached_input_tokens: 0,
            cache_read_tokens: 100_000,
            output_tokens: 0,
            ..Default::default()
        };
        let b = price_usage("o1", &usage);
        assert!(!b.is_complete);
        assert_eq!(b.missing_dimensions, vec![BillableDimension::CacheRead]);
        assert!(b.cache_read_usd > 0.0, "must never be a silent zero");
        assert!(b.total_usd > 0.0);
        // The bound is o1's own uncached input rate: cache reads are a discount
        // everywhere they are published, so this can only over-estimate.
        assert!((b.cache_read_usd - (100_000.0 / 1e6) * 15.00).abs() < 1e-12);

        // Same for an unpublished cache WRITE, bounded by the largest published
        // write premium (2x) rather than by the plain input rate.
        let w = price_usage(
            "amazon.nova-pro-v1:0",
            &BillableUsage {
                cache_write_tokens: 1_000_000,
                ..Default::default()
            },
        );
        assert!(!w.is_complete);
        assert_eq!(w.missing_dimensions, vec![BillableDimension::CacheWrite]);
        assert!((w.cache_write_usd - 0.80 * 2.0).abs() < 1e-12);
        assert!(
            w.cache_write_usd > 0.80,
            "a write is never cheaper than input"
        );
    }

    #[test]
    fn a_missing_dimension_blocks_fail_closed_and_meters_fail_open() {
        let usage = BillableUsage {
            uncached_input_tokens: 10_000,
            cache_read_tokens: 100_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        let incomplete = price_usage("o1", &usage);

        // FAIL-CLOSED: a cap that promises spend cannot exceed a limit has to
        // refuse a call whose spend it cannot compute.
        assert!(incomplete.blocks_fail_closed());
        assert!(!incomplete.is_complete);
        assert_eq!(
            incomplete.missing_dimension_names(),
            vec!["CACHE_READ"],
            "the block reason names the dimension"
        );

        // FAIL-OPEN: the call proceeds, but the conservative total is still
        // metered so cost caps keep advancing. Reporting $0 here is what made
        // cached traffic invisible to caps in the first place.
        assert!(incomplete.total_usd > 0.0);
        let complete = price_usage("claude-sonnet-4-5", &usage);
        assert!(complete.is_complete);
        assert!(!complete.blocks_fail_closed());

        // An unknown model blocks fail-closed for the same reason, even though
        // every dimension it reported was priced at the assumed rate.
        let unknown = price_usage(
            "totally-made-up-model-xyz",
            &BillableUsage::from_tokens(10, 10),
        );
        assert!(unknown.assumed_model_rate);
        assert!(unknown.blocks_fail_closed());
        assert!(unknown.total_usd > 0.0);
        // ...and a cache read on an unknown model flags BOTH.
        let both = price_usage(
            "totally-made-up-model-xyz",
            &BillableUsage {
                cache_read_tokens: 1_000,
                ..Default::default()
            },
        );
        assert!(both.assumed_model_rate);
        assert!(!both.is_complete);
        assert!(both.total_usd > 0.0);
    }

    #[test]
    fn a_reservation_prices_declared_dimensions_and_records_the_version() {
        // Nothing declared: a plain input + max_tokens reservation, matching
        // what the admission path has always reserved.
        let plain = reserve_request_breakdown("gpt-4o", 10, Some(1000), &BillableUsage::default());
        let legacy = estimate_request_cost("gpt-4o", 10, Some(1000)).unwrap();
        assert!((plain.total_usd - legacy).abs() < 1e-12);
        assert!(plain.is_complete);
        assert!(!plain.blocks_fail_closed());
        assert_eq!(plain.pricing_version, CATALOG_VERSION);

        // A declared cache write is reserved at its published premium.
        let with_write = reserve_request_breakdown(
            "claude-opus-4-8",
            1_000,
            Some(100),
            &BillableUsage {
                cache_write_tokens: 200_000,
                ..Default::default()
            },
        );
        assert!((with_write.cache_write_usd - (200_000.0 / 1e6) * 6.25).abs() < 1e-12);
        assert!(with_write.is_complete);

        // A declared dimension the catalog cannot price makes the reservation
        // conservative AND fail-closed-blocking, so admission reaches the same
        // verdict before the call that metering reaches after it.
        let unpriceable = reserve_request_breakdown(
            "amazon.nova-pro-v1:0",
            1_000,
            Some(100),
            &BillableUsage {
                cache_write_tokens: 100_000,
                ..Default::default()
            },
        );
        assert!(!unpriceable.is_complete);
        assert!(unpriceable.blocks_fail_closed());
        assert!(unpriceable.cache_write_usd > 0.0);
        // Without max_tokens the configured assumption applies, as before.
        let assumed = reserve_request_breakdown("gpt-4o", 10, None, &BillableUsage::default());
        assert!(
            (assumed.output_usd - (assumed_output_tokens() as f64 / 1e6) * 10.00).abs() < 1e-12
        );
    }

    #[test]
    fn request_declarations_reserve_anthropic_cache_writes_conservatively() {
        let plain = declared_request_usage(
            "anthropic",
            &json!({"messages": [{"role": "user", "content": "hello"}]}),
            1_000,
        );
        assert!(plain.is_empty());

        let default_ttl = declared_request_usage(
            "AnThRoPiC",
            &json!({
                "cache_control": {"type": "ephemeral"},
                "messages": [{"role": "user", "content": "hello"}]
            }),
            1_000,
        );
        assert_eq!(default_ttl.cache_write_tokens, 1_000);
        assert_eq!(default_ttl.cache_write_1h_tokens, 0);

        let one_hour = declared_request_usage(
            "anthropic",
            &json!({
                "inference_geo": "global",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "a", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                        {"type": "text", "text": "b", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                    ]
                }]
            }),
            1_000,
        );
        assert_eq!(one_hour.cache_write_tokens, 0);
        assert_eq!(one_hour.cache_write_1h_tokens, 1_000);
        assert_eq!(one_hour.token_price_multiplier, None);

        let workspace_may_select_us = declared_request_usage(
            "anthropic",
            &json!({
                "model": "claude-sonnet-5",
                "messages": [{"role": "user", "content": "hello"}]
            }),
            1_000,
        );
        assert_eq!(workspace_may_select_us.token_price_multiplier, Some(1.1));
        let fast_us = declared_request_usage(
            "anthropic",
            &json!({
                "model": "claude-opus-5",
                "speed": "fast",
                "inference_geo": "us",
                "messages": [{"role": "user", "content": "hello"}]
            }),
            1_000,
        );
        assert_eq!(fast_us.token_price_multiplier, Some(2.2));
        let fast_global = declared_request_usage(
            "anthropic",
            &json!({
                "model": "claude-opus-5",
                "speed": "fast",
                "inference_geo": "global",
                "messages": [{"role": "user", "content": "hello"}]
            }),
            1_000,
        );
        assert_eq!(fast_global.token_price_multiplier, Some(2.0));
        let legacy_has_no_residency_multiplier = declared_request_usage(
            "anthropic",
            &json!({
                "model": "claude-sonnet-4-5",
                "messages": [{"role": "user", "content": "hello"}]
            }),
            1_000,
        );
        assert_eq!(
            legacy_has_no_residency_multiplier.token_price_multiplier,
            None
        );

        let non_anthropic = declared_request_usage(
            "openai",
            &json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}}),
            1_000,
        );
        assert!(non_anthropic.is_empty());

        let reserved = reserve_request_breakdown("claude-sonnet-5", 1_000, Some(100), &one_hour);
        assert_eq!(reserved.uncached_input_usd, 0.0);
        assert!((reserved.cache_write_usd - (1_000.0 / 1e6) * 4.0).abs() < 1e-12);
        assert!((reserved.output_usd - (100.0 / 1e6) * 10.0).abs() < 1e-12);
    }

    #[test]
    fn request_declarations_reserve_the_known_openai_web_search_fee() {
        let declared = declared_request_usage(
            "openai",
            &json!({"web_search_options": {"search_context_size": "low"}}),
            10,
        );
        assert_eq!(
            declared.tool_calls,
            vec![("openai:web_search".to_string(), 1)]
        );
        let reserved = reserve_request_breakdown("gpt-4o", 10, Some(10), &declared);
        assert!((reserved.tool_usd - 0.01).abs() < 1e-12);
    }

    #[test]
    fn gpt_5_6_long_context_cache_rates_match_published_prices() {
        for (model, cached_input, cache_write) in [
            ("gpt-5.6-luna", 0.04, 0.50),
            ("gpt-5.6-terra", 0.40, 5.00),
            ("gpt-5.6-sol", 0.80, 10.00),
        ] {
            let rates = rates_at(model, 272_001, at("2026-08-21T00:00:00Z")).unwrap();
            assert_eq!(rates.cached_input_per_1m, Some(cached_input), "{model}");
            assert_eq!(rates.cache_write_per_1m, Some(cache_write), "{model}");
        }

        let b = price_usage(
            "gpt-5.6-sol",
            &BillableUsage {
                uncached_input_tokens: 200_000,
                cache_read_tokens: 50_000,
                cache_write_tokens: 50_000,
                output_tokens: 1_000,
                ..Default::default()
            },
        );
        assert!(b.is_complete);
        assert!(b.missing_dimensions.is_empty());
        assert!((b.uncached_input_usd - 1.60).abs() < 1e-12);
        assert!((b.cache_read_usd - 0.04).abs() < 1e-12);
        assert!((b.cache_write_usd - 0.50).abs() < 1e-12);
        assert!((b.output_usd - 0.03).abs() < 1e-12);
        assert!((b.total_usd - 2.17).abs() < 1e-12);

        // Below the threshold the published short-context rate applies again.
        let short = price_usage(
            "gpt-5.6-sol",
            &BillableUsage {
                uncached_input_tokens: 1_000,
                cache_read_tokens: 1_000,
                ..Default::default()
            },
        );
        assert!(short.is_complete);
        assert!((short.cache_read_usd - (1_000.0 / 1e6) * 0.40).abs() < 1e-12);
    }

    #[test]
    fn total_input_selects_the_long_context_tier_including_cached_tokens() {
        // A cached prefix still occupies the context window, so it counts
        // toward the tier threshold. Ignoring it would price a 300K request at
        // the short-context rate purely because most of it was a cache hit.
        let usage = BillableUsage {
            uncached_input_tokens: 10_000,
            cache_read_tokens: 290_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        assert_eq!(usage.total_input_tokens(), 300_000);
        let b = price_usage("gpt-5.6-luna", &usage);
        assert!((b.uncached_input_usd - (10_000.0 / 1e6) * 0.40).abs() < 1e-12);
        assert!((b.output_usd - (1_000.0 / 1e6) * 1.80).abs() < 1e-12);
    }

    // -- usage parsing ----------------------------------------------------

    #[test]
    fn openai_prompt_tokens_are_inclusive_of_cached_and_written() {
        // Real OpenAI shape: prompt_tokens is the WHOLE prompt, and the cached
        // and written counts are subsets. Charging all three in full would bill
        // the cached prefix twice.
        let body = json!({
            "usage": {
                "prompt_tokens": 100_000,
                "completion_tokens": 500,
                "prompt_tokens_details": { "cached_tokens": 80_000 },
                "cache_write_tokens": 10_000
            }
        });
        let u = parse_usage("gpt-5.6-sol", "openai", &body).unwrap();
        assert_eq!(u.uncached_input_tokens, 10_000);
        assert_eq!(u.cache_read_tokens, 80_000);
        assert_eq!(u.cache_write_tokens, 10_000);
        assert_eq!(u.output_tokens, 500);
        assert_eq!(u.total_input_tokens(), 100_000, "the split is disjoint");

        // The Responses API spells the details object differently.
        let responses = json!({
            "usage": {
                "input_tokens": 1_000,
                "output_tokens": 10,
                "input_tokens_details": { "cached_tokens": 400 }
            }
        });
        let u = parse_usage("gpt-4o", "openai", &responses).unwrap();
        assert_eq!(u.uncached_input_tokens, 600);
        assert_eq!(u.cache_read_tokens, 400);
    }

    #[test]
    fn anthropic_input_tokens_are_exclusive_of_cache_counters() {
        // Anthropic's `input_tokens` EXCLUDES cache reads and cache creation.
        // Subtracting here (as OpenAI requires) would erase real billable
        // input; this is the single most expensive place to get it backwards.
        let body = json!({
            "usage": {
                "input_tokens": 5,
                "output_tokens": 7,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 20
            }
        });
        let u = parse_usage("claude-sonnet-4-5", "anthropic", &body).unwrap();
        assert_eq!(u.uncached_input_tokens, 5);
        assert_eq!(u.cache_read_tokens, 100);
        assert_eq!(u.cache_write_tokens, 20);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.total_input_tokens(), 125);

        // The per-TTL split is preferred when present, because a 1-hour write
        // bills at 2x rather than 1.25x.
        let ttl = json!({
            "usage": {
                "input_tokens": 5,
                "output_tokens": 7,
                "cache_creation_input_tokens": 300,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 100,
                    "ephemeral_1h_input_tokens": 200
                }
            }
        });
        let u = parse_usage("claude-sonnet-4-5", "anthropic", &ttl).unwrap();
        assert_eq!(u.cache_write_tokens, 100);
        assert_eq!(u.cache_write_1h_tokens, 200);
        let b = price_usage("claude-sonnet-4-5", &u);
        let expected = (100.0 / 1e6) * 3.75 + (200.0 / 1e6) * 6.00;
        assert!((b.cache_write_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn anthropic_cache_creation_breakdown_is_disjoint_even_when_one_ttl_is_zero() {
        for (cache_creation, aggregate, expected_5m, expected_1h) in [
            (
                json!({"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 100}),
                100,
                0,
                100,
            ),
            (
                json!({"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 0}),
                100,
                100,
                0,
            ),
            (
                json!({"ephemeral_5m_input_tokens": 40, "ephemeral_1h_input_tokens": 60}),
                100,
                40,
                60,
            ),
        ] {
            let body = json!({
                "usage": {
                    "input_tokens": 1,
                    "cache_creation_input_tokens": aggregate,
                    "cache_creation": cache_creation
                }
            });
            let usage = parse_usage("claude-sonnet-5", "anthropic", &body).unwrap();
            assert_eq!(usage.cache_write_tokens, expected_5m);
            assert_eq!(usage.cache_write_1h_tokens, expected_1h);
            assert_eq!(usage.total_input_tokens(), 101);
        }

        let aggregate_only = json!({
            "usage": {"input_tokens": 1, "cache_creation_input_tokens": 100}
        });
        let usage = parse_usage("claude-sonnet-5", "anthropic", &aggregate_only).unwrap();
        assert_eq!(usage.cache_write_tokens, 100);
        assert_eq!(usage.cache_write_1h_tokens, 0);
    }

    #[test]
    fn anthropic_usage_applies_reported_us_residency_to_every_token_dimension() {
        let body = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 50,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 10,
                    "ephemeral_1h_input_tokens": 40
                },
                "inference_geo": "us",
                "speed": "fast"
            }
        });
        let usage = parse_usage("claude-sonnet-5", "anthropic", &body).unwrap();
        assert_eq!(usage.token_price_multiplier, Some(2.2));
        let cost = price_usage("claude-sonnet-5", &usage);
        let standard = (100.0 / 1e6) * 2.0
            + (20.0 / 1e6) * 10.0
            + (30.0 / 1e6) * 0.2
            + (10.0 / 1e6) * 2.5
            + (40.0 / 1e6) * 4.0;
        assert!((cost.total_usd - standard * 2.2).abs() < 1e-12);
    }

    #[test]
    fn anthropic_pre_output_refusal_keeps_tokens_but_has_zero_cost() {
        let body = json!({
            "stop_reason": "refusal",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 0
            }
        });
        let usage = parse_usage("claude-sonnet-5", "anthropic", &body).unwrap();
        assert!(usage.billing_exempt);
        assert_eq!(usage.uncached_input_tokens, 100);
        assert_eq!(price_usage("claude-sonnet-5", &usage).total_usd, 0.0);

        let billed_partial = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 1,
                "unbilled_refusal": true
            }
        });
        let usage = parse_usage("claude-sonnet-5", "anthropic", &billed_partial).unwrap();
        assert!(!usage.billing_exempt, "partial-output refusals are billed");
        assert!(price_usage("claude-sonnet-5", &usage).total_usd > 0.0);
    }

    #[test]
    fn the_gateways_anthropic_stream_translation_keeps_exclusive_semantics() {
        // `providers::anthropic_stream` emits an OpenAI-shaped final chunk in
        // which `prompt_tokens` is Anthropic's non-cache input and the cache hit
        // sits in `prompt_tokens_details.cached_tokens`. Reading that body with
        // OpenAI's inclusive rule would zero out the uncached input entirely.
        let translated = json!({
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 7,
                "total_tokens": 12,
                "prompt_tokens_details": { "cached_tokens": 100 }
            }
        });
        let u = parse_usage("claude-sonnet-4-5", "anthropic", &translated).unwrap();
        assert_eq!(u.uncached_input_tokens, 5, "must not be subtracted away");
        assert_eq!(u.cache_read_tokens, 100);
        // The same bytes read as OpenAI would have produced 0 uncached input.
        let as_openai = parse_usage("gpt-4o", "openai", &translated).unwrap();
        assert_eq!(as_openai.uncached_input_tokens, 0);
    }

    #[test]
    fn bedrock_cache_counters_are_read_and_treated_as_exclusive() {
        // AWS: "the `inputTokens` field represents only the non-cached input
        // tokens ... total input tokens = inputTokens + cacheReadInputTokens +
        // cacheWriteInputTokens".
        let body = json!({
            "usage": {
                "inputTokens": 1_000,
                "outputTokens": 200,
                "cacheReadInputTokens": 4_000,
                "cacheWriteInputTokens": 500
            }
        });
        let u = parse_usage(
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "bedrock",
            &body,
        )
        .unwrap();
        assert_eq!(u.uncached_input_tokens, 1_000);
        assert_eq!(u.cache_read_tokens, 4_000);
        assert_eq!(u.cache_write_tokens, 500);
        assert_eq!(u.total_input_tokens(), 5_500);
    }

    #[test]
    fn bedrock_anthropic_request_reservation_distinguishes_global_and_regional_profiles() {
        let cases = [
            (
                "anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some(1.1),
                0.005775,
            ),
            (
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some(1.1),
                0.005775,
            ),
            (
                "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                None,
                0.00525,
            ),
            (
                "arn:aws:bedrock:us-east-1:111122223333:inference-profile/global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                None,
                0.00525,
            ),
            (
                "arn:aws:bedrock:ap-southeast-2:111122223333:inference-profile/apac.anthropic.claude-sonnet-4-5-20250929-v1:0",
                Some(1.1),
                0.005775,
            ),
        ];
        for (model, multiplier, expected_total) in cases {
            let mut declared = declared_request_usage("bedrock", &json!({"model": model}), 1_000);
            assert_eq!(declared.token_price_multiplier, multiplier, "{model}");
            // A strict reservation that knows the prompt will populate the
            // provider cache must apply the same geo rate to the write premium.
            declared.cache_write_tokens = 1_000;
            let reserved = reserve_request_breakdown(model, 1_000, Some(100), &declared);
            assert!(!reserved.assumed_model_rate, "{model}");
            assert_eq!(reserved.uncached_input_usd, 0.0, "{model}");
            assert!(
                (reserved.total_usd - expected_total).abs() < 1e-12,
                "{model}: {:?}",
                reserved
            );
        }

        let nova =
            declared_request_usage("bedrock", &json!({"model": "amazon.nova-pro-v1:0"}), 1_000);
        assert_eq!(nova.token_price_multiplier, None);
        let nova_reserved =
            reserve_request_breakdown("amazon.nova-pro-v1:0", 1_000, Some(100), &nova);
        assert!((nova_reserved.total_usd - 0.00112).abs() < 1e-12);
    }

    #[test]
    fn bedrock_anthropic_settlement_multiplies_input_output_and_cache_dimensions() {
        let response = json!({
            "usage": {
                "inputTokens": 1_000,
                "outputTokens": 200,
                "cacheReadInputTokens": 4_000,
                "cacheWriteInputTokens": 500
            }
        });

        let direct_model = "anthropic.claude-haiku-4-5-20251001-v1:0";
        let direct_usage = parse_usage(direct_model, "bedrock", &response).unwrap();
        assert_eq!(direct_usage.token_price_multiplier, Some(1.1));
        let direct = price_usage(direct_model, &direct_usage);
        assert!(!direct.assumed_model_rate);
        assert!((direct.uncached_input_usd - 0.0011).abs() < 1e-12);
        assert!((direct.cache_read_usd - 0.00044).abs() < 1e-12);
        assert!((direct.cache_write_usd - 0.0006875).abs() < 1e-12);
        assert!((direct.output_usd - 0.0011).abs() < 1e-12);
        assert!((direct.total_usd - 0.0033275).abs() < 1e-12);

        let global_model = "global.anthropic.claude-haiku-4-5-20251001-v1:0";
        let global_usage = parse_usage(global_model, "bedrock", &response).unwrap();
        assert_eq!(global_usage.token_price_multiplier, None);
        let global = price_usage(global_model, &global_usage);
        assert!(!global.assumed_model_rate);
        assert!((global.total_usd - 0.003025).abs() < 1e-12);
        assert!((direct.total_usd - global.total_usd * 1.1).abs() < 1e-12);
    }

    #[test]
    fn server_tool_use_and_search_counts_become_tool_calls() {
        let anthropic = json!({
            "usage": {
                "input_tokens": 10,
                "output_tokens": 10,
                "server_tool_use": { "web_search_requests": 4, "web_fetch_requests": 1 }
            }
        });
        let u = parse_usage("claude-sonnet-4-5", "anthropic", &anthropic).unwrap();
        assert_eq!(
            u.tool_calls,
            vec![
                ("anthropic:web_search".to_string(), 4),
                ("anthropic:web_fetch".to_string(), 1),
            ]
        );
        let b = price_usage("claude-sonnet-4-5", &u);
        assert!((b.tool_usd - 0.04).abs() < 1e-12);
        assert!(b.is_complete);

        let pplx = json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 100,
                "num_search_queries": 2,
                "search_context_size": "high"
            }
        });
        let u = parse_usage("sonar-pro", "perplexity", &pplx).unwrap();
        assert_eq!(
            u.tool_calls,
            vec![("perplexity:sonar-pro:search_high".to_string(), 2)]
        );
        let b = price_usage("sonar-pro", &u);
        assert!((b.tool_usd - 0.028).abs() < 1e-12, "2 at $14/1k");
    }

    #[test]
    fn a_body_without_usage_parses_to_none() {
        assert!(parse_usage("gpt-4o", "openai", &json!({ "choices": [] })).is_none());
        assert!(parse_usage("gpt-4o", "openai", &json!({ "usage": null })).is_none());
        // An empty usage object is a real (zero) reading, not a missing one.
        let u = parse_usage("gpt-4o", "openai", &json!({ "usage": {} })).unwrap();
        assert!(u.is_empty());
        assert_eq!(price_usage("gpt-4o", &u).total_usd, 0.0);
    }

    #[test]
    fn an_unrecognized_provider_over_counts_rather_than_under_counts() {
        // Unknown providers are treated as exclusive, which charges the cached
        // prefix at full input on top of the cache rate. That over-estimates,
        // which is the safe direction for a cap.
        let body = json!({
            "usage": {
                "prompt_tokens": 1_000,
                "completion_tokens": 0,
                "prompt_tokens_details": { "cached_tokens": 900 }
            }
        });
        let unknown = parse_usage("gpt-4o", "brand-new-provider", &body).unwrap();
        let known = parse_usage("gpt-4o", "openai", &body).unwrap();
        assert_eq!(unknown.uncached_input_tokens, 1_000);
        assert_eq!(known.uncached_input_tokens, 100);
        assert!(
            price_usage("gpt-4o", &unknown).total_usd > price_usage("gpt-4o", &known).total_usd
        );
    }

    #[test]
    fn one_deduped_alert_channel_covers_both_unpriced_reasons() {
        // NOV-152 added a deduped warning for an unknown model id. A missing
        // billable dimension is the same class of problem and must not get a
        // second, independently-throttled channel: one hot model would then be
        // able to flood the log through the new one, and an operator would have
        // two places to point an alert at. Both reasons share this gate, keyed
        // per (model, reason) so a model that hits both still says so twice.
        let model = "dedup-probe-model-9f3a";
        let id = canonical(model);
        assert!(warn_incomplete_pricing(&id, "unknown-model", || {}));
        assert!(
            !warn_incomplete_pricing(&id, "unknown-model", || {}),
            "the same reason must not warn twice for one model"
        );
        assert!(
            warn_incomplete_pricing(&id, BillableDimension::CacheRead.as_str(), || {}),
            "a different reason on the same model is its own alert"
        );
        assert!(!warn_incomplete_pricing(
            &id,
            BillableDimension::CacheRead.as_str(),
            || {}
        ));
        // A different model warns on its own.
        assert!(warn_incomplete_pricing(
            "dedup-probe-model-other-9f3a",
            "unknown-model",
            || {}
        ));
    }

    #[test]
    fn breakdown_serializes_the_component_shape_the_platform_reads() {
        let b = price_usage(
            "o1",
            &BillableUsage {
                uncached_input_tokens: 1_000,
                cache_read_tokens: 1_000,
                output_tokens: 100,
                ..Default::default()
            },
        );
        let v = serde_json::to_value(&b).unwrap();
        for key in [
            "uncachedInputUsd",
            "cacheReadUsd",
            "cacheWriteUsd",
            "outputUsd",
            "toolUsd",
            "totalUsd",
            "isComplete",
            "missingDimensions",
            "pricingVersion",
            "assumedModelRate",
            "source",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["isComplete"], false);
        assert_eq!(v["missingDimensions"][0], "CACHE_READ");
        assert_eq!(v["source"], "CATALOG");
        assert_eq!(v["pricingVersion"], CATALOG_VERSION);
        assert!(v["totalUsd"].as_f64().unwrap() > 0.0);
    }
}
