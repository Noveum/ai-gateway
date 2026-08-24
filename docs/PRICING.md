# Model pricing and Nova Guard cost accounting

The gateway's checked-in pricing catalog is version **`2026.08.23`**. All token
rates are USD per 1,000,000 tokens. Nova Guard uses these rates to reserve and
settle cost-cap usage; they are policy estimates, not a provider invoice.

The complete, reviewable manifest is [`pricing/catalog.json`](../pricing/catalog.json).
It contains 161 current and legacy base rows, cache rates, long-context tiers,
scheduled changes, tool fees, source links, aliases, and provenance. This guide
describes the accounting contract and highlights current rows instead of
duplicating that entire manifest.

## Sources of truth

| Concern | Location |
|---|---|
| Versioned base/cache rates, long-context and scheduled rows, aliases, tool fees, and primary-source URLs | [`pricing/catalog.json`](../pricing/catalog.json) |
| Gateway runtime mirror used by the compiled binary | [`src/policy/pricing_catalog.rs`](../src/policy/pricing_catalog.rs) |
| Lookup, conservative fallbacks, request reservation, usage parsing, and settlement arithmetic | [`src/policy/pricing.rs`](../src/policy/pricing.rs) |
| Platform billing mirror | `packages/telemetry/src/pricing/generated/index.ts` in the Noveum platform repository |

`pricing/catalog.json` is the rate-review manifest. The Rust and platform
tables are currently hand-maintained mirrors, not generated artifacts. A rate
change is incomplete until all applicable copies, tests, and the catalog
version agree.

## What one cost contains

For catalog-priced usage, the total is:

```text
uncached input
+ cache reads
+ cache writes (default TTL and Anthropic 1-hour TTL separately)
+ output
+ known per-call tool/search fees
```

Long-context tiers are selected using the total input context, including cache
reads and writes. A request/response multiplier, such as Anthropic US-only
inference or fast mode, applies to every token and cache dimension but not to a
separately priced tool call.

Every breakdown records the catalog version, whether all used dimensions were
priced, which dimensions were missing, whether an unknown-model assumption was
used, and whether the total came from catalog arithmetic or an authoritative
provider-reported charge. For `xai` / `grok`, a supplied
`usage.cost_in_usd_ticks` is authoritative: xAI defines 10 billion ticks as one
USD and documents the total as including cache discounts and server-side tools.
When that field is absent, the gateway falls back to catalog arithmetic. No
other provider is configured as an authoritative cost source in catalog version
`2026.08.23`.

## Lookup and defensive pricing

Lookup lowercases the model ID and applies these rules:

1. An exact model ID wins.
2. A declared exact alias is resolved to its concrete target.
3. Otherwise the longest catalog family extended at a version boundary (`-`,
   `:`, `.`, `@`, or `/`) wins. For example, a dated `gpt-4o-*` snapshot can
   match `gpt-4o`, while `gpt-4oxyz` cannot.

The current exact aliases are `gpt-5.6` and `daybreak-blue-latest` to
`gpt-5.6-sol`, plus `daybreak-red-latest` to `gpt-5.6-cyber`.

An unknown model is **not free and is not globally rejected**. Nova Guard
prices it at the maximum input and output rates derived from the compiled Rust
runtime's base, long-context, and scheduled rows (currently $15/$75 per
million), marks the breakdown as assumed, and emits a deduplicated warning.
This lets the proxy pass new provider model IDs while preventing an
unrecognized ID from reserving $0. A `cost_cap` with `failClosed: true` blocks
an unknown model lookup. For a known model, an unpriced cache/tool dimension is
conservatively charged and marked incomplete, but incompleteness alone does not
currently trigger the policy's fail-closed branch.

When a provider reports a billable dimension whose rate is missing, the
gateway also refuses to silently drop it:

- an unpriced cache read uses that model's uncached-input rate;
- an unpriced cache write uses that input rate times the largest catalogued
  cache-write premium (currently 2x);
- an unknown tool uses the largest catalogued per-call tool fee.

The breakdown is marked incomplete in each case. A low-level catalog-only
helper may use `0` to mean "no row" internally, but the reservation, policy,
telemetry, and settlement boundaries replace that sentinel with this defensive
breakdown. It is never recorded as a real zero-dollar unknown call.

## Reservation and settlement

Nova Guard builds a preflight hold from its current request-token estimate,
explicit output ceiling, and declared billable dimensions, then settles against
provider-reported usage:

- An applicable enforcing, blocking, strict `cost_cap` requires a positive
  explicit `max_tokens`, `max_completion_tokens`, or `max_output_tokens`.
  Multiple non-null aliases must agree; strict admission rejects a conflict and
  normalizes compatible-provider requests to one upstream ceiling.
  Unbounded strict requests receive HTTP 400 `missing_output_limit` before a
  reservation or provider call. Advisory caps and rate-only policies may use
  `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` (default `1024`).
- The strict contract is intentionally narrower than transparent proxying: it
  accepts bounded JSON `/v1/chat/completions` requests with a model and messages.
  It rejects Responses/conversation state, images, files, audio, remote search
  (including xAI `search_parameters`), server/MCP tools, multi-choice
  `n`/`best_of`, Perplexity, and OpenRouter before admission. Client function
  tools remain supported on adapters that translate or transparently pass them;
  Bedrock tool use is not translated yet. Strict Bedrock admission is limited
  to catalogued commercial Claude IDs/profiles whose global or geographic
  multiplier is derivable from the model ID; region-priced Nova/Titan families
  are rejected before reservation until source Region is part of the pricing
  contract. Direct OpenAI strict calls are pinned
  to `service_tier: "default"`; premium service tiers are rejected.
  Advisory policies retain the broader proxy surface. Because a multipart or
  otherwise non-JSON body has no trustworthy model with which to evaluate
  `scopeToModels`, the presence of any enforcing/blocking strict cap rejects
  that opaque shape instead of guessing that it is out of scope.
- Declaring Anthropic prompt caching reserves the **entire estimated prompt**
  as a first-write miss at the longest declared TTL. A `1h` breakpoint
  therefore reserves the 2x input write rate; omitted TTL means `5m` (1.25x).
  This is intentionally conservative even when only part of the prompt has a
  breakpoint.
- For eligible Anthropic models, `inference_geo: "global"` reserves standard
  rates and `"us"` reserves 1.1x. If it is omitted, admission reserves 1.1x
  because the workspace can select US-only inference; provider-reported geo
  settles the hold to the exact multiplier.
- Anthropic `speed: "fast"` is accepted only for `claude-opus-5` and
  `claude-opus-4-8` and reserves 2x token/cache rates. It stacks
  multiplicatively with US-only inference, so the combined reservation and
  settlement multiplier is 2.2x. `speed: "standard"` and omission use the
  normal rate. Invalid values and fast mode on other models are rejected before
  admission. The caller must also opt into Anthropic's research preview with
  `anthropic-beta: fast-mode-2026-02-01`; the gateway forwards rather than
  synthesizes that header.
- A supported request-declared tool fee, such as OpenAI web search, is included
  in the reservation. Provider-reported tool counts are included in settlement
  when the request shape itself is supported.
- An Anthropic pre-output refusal (`stop_reason: "refusal"` with zero output
  tokens) retains token counts for observability but settles monetary cost to
  **$0**. A refusal after any output is billed normally.
- A response with explicit zero output is authoritative and completes the
  reservation. Buffered and streaming settlement both require authoritative
  input **and** output token fields; missing usage or either missing half
  abandons and retains the conservative estimate.

## Current catalog highlights

These rows are a small operational subset of the full manifest. Cache columns
mean cache hit, default cache write, and Anthropic 1-hour cache write
respectively; `—` means the dimension is not present for that row.

### OpenAI GPT-5.6 family

| Model | Input | Output | Cache hit | Cache write |
|---|---:|---:|---:|---:|
| `gpt-5.6-luna` | $0.20 | $1.20 | $0.02 | $0.25 |
| `gpt-5.6-terra` | $2.00 | $12.00 | $0.20 | $2.50 |
| `gpt-5.6-sol` | $4.00 | $20.00 | $0.40 | $5.00 |
| `gpt-5.6-cyber` | $12.50 | $75.00 | $1.25 | $15.625 |

The current `gpt-5.6` and `daybreak-blue-latest` aliases resolve to
`gpt-5.6-sol`; `daybreak-red-latest` resolves to `gpt-5.6-cyber`.

OpenAI describes the GPT-5.6 Sol Standard rates above as promotional and
available **at least through November 21, 2026**. It has not published an exact
end date or replacement rates, so the catalog carries the promotion as the
current rate and intentionally has no scheduled rollback.

### Selected Anthropic catalog rows

These are selected non-legacy pricing rows, not an inventory of models enabled
for a particular Anthropic account. Verify model availability separately.

| Model | Catalog note | Input | Output | Cache hit | 5m write | 1h write |
|---|---|---:|---:|---:|---:|---:|
| `claude-sonnet-5` | Non-legacy row; no scheduled increase | $2.00 | $10.00 | $0.20 | $2.50 | $4.00 |
| `claude-opus-5` | Non-legacy row | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-opus-4-8` | Non-legacy row | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-opus-4-5-20251101` | Non-legacy dated row | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-fable-5` | Non-legacy row | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |
| `claude-mythos-5` | Non-legacy row | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |

Anthropic publishes cache hits at 0.1x input, 5-minute writes at 1.25x,
1-hour writes at 2x, US-only inference at 1.1x for eligible models, and fast
mode at 2x for supported models. Sonnet 5's previously announced increase to
$3/$15 was cancelled; the catalog intentionally has no scheduled increase.

### Long-context tiers

When total input exceeds the threshold, the whole request uses the tier row:

| Model | Threshold | Input | Output | Cache hit | Cache write |
|---|---:|---:|---:|---:|---:|
| `gpt-5.6-luna` | >272,000 | $0.40 | $1.80 | $0.04 | $0.50 |
| `gpt-5.6-terra` | >272,000 | $4.00 | $18.00 | $0.40 | $5.00 |
| `gpt-5.6-sol` | >272,000 | $8.00 | $30.00 | $0.80 | $10.00 |
| `gemini-2.5-pro` | >200,000 | $2.50 | $15.00 | $0.25 | $0.00 |
| `grok-4.3` | >200,000 | $2.50 | $5.00 | $0.40 | $0.00 |

Long-context rows are encoded under `longContext` in `pricing/catalog.json` and
mirrored into both the Rust runtime table and the platform TypeScript pricing
table. Both select the long-context row only when total input is greater than
the documented threshold; the exact threshold remains on the short rate.

## Anthropic request limitations that protect cost accuracy

The Anthropic adapter rejects unsupported cost-affecting shapes before Nova
Guard admission:

- `fallbacks` is rejected because one response can bill multiple models while
  the current reservation and settlement record has one model.
- `speed: "fast"` is limited to `claude-opus-5` and `claude-opus-4-8`, whose
  2x pricing is modeled. Standard speed remains supported.
- On constrained Claude families, non-default `temperature`, non-default
  `top_p`, and any `top_k` are rejected instead of relying on an upstream 400.
- Sonnet 5 rejects manual `thinking.type: "enabled"` (use `adaptive` or
  `disabled`) and a final assistant prefill.
- Every supplied `cache_control` must be an object with `type: "ephemeral"`
  and an omitted, `5m`, or `1h` TTL.
- Native Anthropic server tools and MCP tool definitions remain unsupported by
  this OpenAI Chat Completions adapter. The settlement parser can account for
  server-tool usage fields if present, but that does not make those request
  shapes supported.

See [the Anthropic compatibility guide](providers/anthropic.md) for the exact
request and response contract.

## Accuracy boundaries

- Published batch discounts are not modeled by the synchronous gateway path.
- OpenAI Fast/Priority pricing and the regional-processing 10% uplift are not
  modeled in settlement. Strict direct-OpenAI admission pins
  `service_tier: "default"` and rejects a requested premium tier; advisory
  traffic can still forward premium tiers, and a regional base URL can still
  differ from the catalog estimate.
- Catalogued Bedrock Claude rows use global rates. Direct and geography-scoped
  Claude 4.5 IDs (including identifiable system inference-profile ARNs) are
  reserved and settled at 1.1x across input, output, and cache dimensions;
  explicit `global.` profiles use the global rate. Opaque application inference
  profiles remain unpriceable rather than being guessed.
- AWS publishes source-Region-specific rates for Amazon Nova (for example, the
  public 2026-08-20 price list prices Nova Pro in Milan above the catalog's
  global rate). Nova/Titan requests therefore remain supported for advisory
  proxying, but strict Bedrock admission rejects them until reservation and
  settlement receive the validated source Region.
- Gemini cached-content storage is billed over time, but a single response does
  not report the storage duration; the per-request catalog therefore records a
  zero write-token charge and does not claim to model storage.
- Negotiated discounts, taxes, currency conversion, provider rounding, and
  later price changes can differ from the catalog.
- OpenRouter and other routing providers are exact only when the resolved model
  maps to a catalog row or an authoritative billed total becomes available.
- xAI/Grok settlement prefers `usage.cost_in_usd_ticks` when the provider
  supplies it; a response without that field falls back to the catalog and its
  normal completeness caveats. Strict requests reject `search_parameters`
  because its server-search fee cannot be bounded by a token-only hold.
- Legacy rows are preserved for compatibility and explicitly marked
  `legacy: true`; do not assume their rates were reverified for this catalog
  version.

## Examples

An uncached Sonnet 5 call with 1,000 input and 500 output tokens costs:

```text
(1,000 / 1,000,000 × $2) + (500 / 1,000,000 × $10) = $0.007
```

If the same token counts are reported with US-only inference, the settled cost
is `$0.007 × 1.1 = $0.0077`. Sonnet 5 does not support fast mode in the gateway.
For an Opus 5 call, fast mode doubles all token/cache dimensions; fast plus US
inference multiplies them by `2 × 1.1 = 2.2`.

For a Sonnet 5 request whose estimated 1,000-token prompt declares a 1-hour
cache breakpoint and whose maximum output is 500 tokens, strict admission
reserves:

```text
(1,000 / 1,000,000 × $4) + (500 / 1,000,000 × $10) = $0.009
```

If the request omits `inference_geo`, the conservative reservation is
`$0.009 × 1.1 = $0.0099`; settlement reconciles it after Anthropic reports the
actual geo and cache usage.

## Updating prices

1. Verify the change against the provider's primary pricing and model-status
   documentation.
2. Edit `pricing/catalog.json` first, preserving a primary-source URL and
   bumping its version.
3. Mirror base/cache/tool changes in `src/policy/pricing_catalog.rs` and the
   platform TypeScript table. Update long-context or scheduled rows in both
   runtime mirrors where applicable.
4. Add or update exact-value, family-boundary, cache, tier, reservation, and
   settlement tests. A future scheduled price must also prove its activation
   boundary and cache-rate rescaling.
5. Update this guide only for behavior or high-value highlights; the JSON
   manifest remains the complete row list.

## Primary references

- [OpenAI API pricing](https://developers.openai.com/api/docs/pricing)
- [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [Anthropic current model table](https://platform.claude.com/docs/en/about-claude/models/overview)
- [Anthropic model deprecations](https://platform.claude.com/docs/en/about-claude/model-deprecations)
- [Google Gemini API pricing](https://ai.google.dev/gemini-api/docs/pricing)
- [xAI models and pricing](https://docs.x.ai/docs/models)
- [xAI exact per-request cost tracking](https://docs.x.ai/developers/cost-tracking)
- [Perplexity pricing](https://docs.perplexity.ai/getting-started/pricing)
