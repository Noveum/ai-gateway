# PR #30 NovaGuard release review

**Review date:** 2026-08-23

**Gateway PR:** [Noveum/ai-gateway#30](https://github.com/Noveum/ai-gateway/pull/30)

**Gateway working branch:** `codex/pr30-release-blockers` (published to PR head
`feat/novaguard-platform-bridge`)

**Companion app PR:** [Noveum/noveum-app-nextjs#544](https://github.com/Noveum/noveum-app-nextjs/pull/544)

**Companion app branch:** `codex/pr30-platform-guardrails`

## Executive verdict

The corrected local implementation is a **GO in the audited code scope**. No
reproducible P0 or P1 correctness blocker remains in the frozen local tree.
NovaGuard works end to end across the native gateway and a real local
Cloudflare Workerd runtime, including atomic admission, strict cost limits,
organization counters, request transforms, provider streaming, usage
settlement, concurrency, and reservation cleanup.

At the start of this blocker-fix pass, GitHub pointed at gateway commit
`a0e93f8` and companion-app commit `5b7b14d`; their published checks were green,
but they did not include the final Worker cache/configuration, pricing,
model-scope-contract, and workflow-hardening changes described here. Approval
should happen only after CI passes on both final pushed SHAs, the companion app
is deployed first, and a fresh human/bot review is requested.

Cloudflare compatibility is proven by WASM compilation, pinned
`worker-build 0.8.5`, Wrangler `4.120.0 deploy --dry-run`, and the full 13-phase
Workerd suite. A live Cloudflare deployment was intentionally not created, so
this document does not claim production-edge deployment evidence.

## What belongs in this PR

### 1. Strict NovaGuard admission is now a real bounded contract

Primary files:

- `src/policy/middleware.rs`
- `src/worker_rt.rs`
- `src/routing.rs`
- `src/policy/admission.rs`
- `src/policy/admission_wire.rs`
- `src/policy/worker_remote.rs`
- `src/policy/engine.rs`
- `tests/novaguard_platform.rs`
- `scripts/novaguard_worker_e2e.sh`

Required behavior now implemented:

- Applicable `mode=enforce`, blocking, strict cost caps require a positive,
  explicit output ceiling before `/admit`.
- `max_tokens`, `max_completion_tokens`, and `max_output_tokens` are accepted;
  multiple non-null aliases must agree.
- The forwarded provider ceiling is the same ceiling reserved by NovaGuard.
- Admission estimates the complete post-transform serialized JSON body, not
  only visible message text. Function-tool schemas receive an additional
  conservative reserve.
- Input-transform expansion is measured after transformation and rechecked
  against the 8 MiB body limit.
- Strict mode rejects unbounded or externally expanded work before admission:
  Responses/conversation state, malformed or non-JSON bodies, remote media,
  file/audio/video/document inputs, server/MCP tools, provider search, premium
  service tiers, Perplexity, and OpenRouter.
- Advisory, shadow, and model-scope-miss cost policies are not accidentally
  promoted to strict. When any stateful cost/rate policy is active, opaque or
  malformed bodies are rejected because forwarding them without admission
  would bypass the authoritative counters.
- Provider-local deterministic validation and authentication errors happen
  before a reservation is created.
- Unknown providers are rejected before `/state`, `/admit`, or provider fetch.

### 1a. Shared-gateway authorization and admission match the platform

Primary gateway files:

- `src/policy/remote.rs`
- `src/policy/middleware.rs`
- `src/policy/engine.rs`
- `src/policy/admission_wire.rs`
- `src/worker_rt.rs`

Primary companion-app files:

- `packages/api/src/routes/v1/projects/router.ts`
- `packages/api/src/routes/v1/guardrails/schemas.ts`
- `packages/api/src/routes/v1/guardrails/admission-handlers.ts`
- `packages/billing/src/guardrails-admission.ts`

The original shared runtime cached the first caller's authorization-bearing
clients by organization/project and reused them for later API keys selecting
the same tenant. The corrected runtime caches per-credential clients by a full
SHA-256 identity, bounds rotated-key runtimes to 64 with idle/LRU eviction, and
still shares one tenant-wide pending-spend ledger so two valid keys cannot spend
the same advisory headroom.

The gateway now resolves credentials through
`GET /api/v1/projects/guardrails/resolve`. The companion endpoint requires all
three permissions synchronously: `projects:read`, `guardrails:read`, and
`guardrails:ingest`. A read-only key is rejected before any provider request,
rather than being allowed while asynchronous usage ingestion fails later.

The atomic admission wire now carries optional `forceStrictCostCaps`. Stored
strict cost caps and every rate limit participate by default; the native strict
deployment override forces all applicable cost caps. Cost model scopes are
matched exactly and case-insensitively on both sides. Admission-outage handling
uses the same policy selection, including rate-limit `failClosed` behavior.

Strict mode deliberately favors an explicit 400 over a false “hard cap.” The
broader provider surface remains available under advisory policies.

### 2. Organization guardrails use organization counters

Primary gateway files:

- `src/policy/engine.rs`
- `src/policy/remote.rs`
- `src/policy/platform.rs`

Primary platform files:

- `packages/billing/src/guardrails-admission.ts`
- `packages/api/src/routes/v1/guardrails/admission-handlers.ts`
- `packages/api/tests/v1/guardrails-admission.integration.spec.ts`
- `packages/api/tests/v1/guardrails-admission-model-scope.spec.ts`

The gateway no longer substitutes project cost/rate windows when an
organization-scoped policy needs organization state. Missing organization state
uses unavailable/fail-closed semantics instead of silently enforcing the wrong
counter. The platform admission path uses the policy source for the atomic
counter key, and the companion patch filters `scopeToModels` by exact,
case-insensitive model match before calculating reservation operations.

The previously accepted but unimplemented `1d_calendar` window is now backed
end to end. Redis admission and reads sum UTC-midnight through the current hour;
the Postgres fallback sums from UTC midnight; both organization and project
state expose the same key. The dashboard can create and edit this window.

Production verification used two projects in the same organization. Their
project counters remained distinct while the returned 30-day organization
counter was identical. A real guarded GPT-5.6 Luna request increased the
project, organization, and model counters by the same measured
`$0.0000068`. This is the evidence needed to close the current organization
counter review thread after the fixes are pushed.

### 3. Anthropic is normalized on native and Worker paths

Primary files:

- `src/routing.rs`
- `src/providers/anthropic.rs`
- `src/providers/anthropic_stream.rs`
- `src/worker_rt.rs`
- `docs/providers/anthropic.md`

The same shared transformer now drives native and Worker requests. It covers:

- output-limit aliases and a deterministic default for non-strict requests;
- ordered system/developer-message hoisting;
- `stop` to `stop_sequences`;
- OpenAI function tools, tool choice, parallel-tool control, assistant tool
  calls, and tool results;
- image data blocks supported by the compatibility contract;
- sampling/model validation for current constrained Claude families;
- Anthropic auth, `anthropic-beta`, safe custom headers, and query forwarding;
- buffered tool calls and `finish_reason=tool_calls`;
- fragmented Anthropic SSE, parallel tools, server/MCP deltas, exactly one
  terminal chunk, `[DONE]`, and terminal usage;
- cache read/write counters, server-tool counts, inference geography, speed,
  and pre-output refusal semantics for settlement.

Unsupported mixed-model `fallbacks`, native server/MCP tools, and unsupported
premium shapes are rejected deterministically before admission rather than
being forwarded and mispriced.

This remains an OpenAI-shaped gateway contract. It does not claim wire-level
compatibility with every Anthropic SDK response type.

### 4. Pricing and settlement are billing-aware

Primary files:

- `src/policy/pricing.rs`
- `src/policy/pricing_catalog.rs`
- `pricing/catalog.json`
- `src/policy/metering.rs`
- `docs/PRICING.md`

Catalog version `2026.08.23` now covers current model IDs, aliases, cache rates,
long-context tiers, tool/search fees, and provider multipliers used by this PR.
Request admission reserves declared cache/tool premiums, while completion uses
provider-reported detailed usage. Important fixes include:

- disjoint Anthropic 5-minute and 1-hour cache-write counters;
- cache-read and cache-write rates, not ordinary input rates;
- Anthropic US inference and supported fast-mode multipliers;
- zero-cost pre-output Anthropic refusals, while mid-output refusals remain
  billable;
- exact xAI `usage.cost_in_usd_ticks` when present and hidden reasoning-token
  fallback when it is absent;
- conservative non-zero pricing for unknown models/dimensions;
- no fabricated `0/0` usage when a provider reports only one token half;
- GPT-5.6 long-context and cache tiers;
- Bedrock Claude global versus geographic/direct pricing in both reservation
  and settlement.

Strict Bedrock admission accepts only catalogued commercial Claude model or
system-profile IDs whose rate is derivable from the model ID. Amazon Nova,
Titan, opaque application profiles, and non-commercial partition profiles stay
available for advisory proxying but are rejected before strict admission until
validated AWS source Region is part of the pricing contract. This is necessary
because AWS's 2026-08-20 public price list, for example, prices Nova Pro in
`eu-south-1` at `$1.28/$5.21` per million input/output tokens rather than the
catalog's `$0.80/$3.20` global row.

### 5. Cloudflare Worker parity and lifecycle safety

Primary files:

- `src/worker_rt.rs`
- `src/policy/worker_remote.rs`
- `scripts/novaguard_mock_platform.py`
- `scripts/novaguard_worker_e2e.sh`
- `docs/CLOUDFLARE_WORKER.md`
- `docs/CLOUDFLARE_DEPLOYMENT.md`

The Worker now has the platform policy/state/admission bridge, shared request
normalization, provider-aware settlement, exact SSE scanning, and a ten-minute
maximum admitted upstream lifetime. The deadline is intentionally below the
platform's fifteen-minute reservation lease, so an active Worker stream cannot
outlive and then lose its atomic hold to the platform reaper.

The final Workerd run used 16 concurrent clients across four traffic classes:
OpenAI buffered, OpenAI streaming, Anthropic buffered, and Anthropic streaming.
It observed 16 new admissions, 16 provider calls, 16 completions, zero replay,
zero cancellation, zero abandonment, zero HTTP errors, zero active holds, and
`reservedUsd=0` after settlement. Peak provider concurrency was 16.

### 6. Bedrock transport correctness

Primary files:

- `src/providers/bedrock.rs`
- `src/proxy/mod.rs`
- `src/routing.rs`
- `docs/providers/bedrock.md`

AWS EventStream content type is preserved until the Bedrock decoder sees it;
binary messages are reassembled across arbitrary HTTP transport fragmentation,
then emitted as valid, separately framed OpenAI SSE events. Bedrock tools and
multimodal content are explicitly documented as unsupported by the current
OpenAI-to-Converse adapter and are rejected in strict mode.

### 7. Provider smoke CI actually starts the gateway

Primary files:

- `.github/workflows/provider-smoke.yml`
- `tests/integration/common.rs`
- `tests/integration/anthropic_test.rs`
- `tests/integration/groq_test.rs`
- `tests/integration/together_test.rs`
- `tests/README.md`

The workflow builds one locked gateway binary, starts it, polls `/health`, runs
provider tests serially, and always cleans it up. Retired fixtures were replaced
with active models: `claude-sonnet-5`, `openai/gpt-oss-20b`, and
`meta-llama/Llama-3.3-70B-Instruct-Turbo`. Streaming assertions now require
`[DONE]` while permitting valid tool-call-only streams.

## Current model and price coverage

The following standard rates are in both the Rust catalog and the companion
TypeScript pricing table. Amounts are USD per one million tokens.

| Provider | Model | Input | Output | Cache read | Cache write | 1h cache write |
|---|---:|---:|---:|---:|---:|---:|
| OpenAI | `gpt-5.6-luna` | $0.20 | $1.20 | $0.02 | $0.25 | — |
| OpenAI | `gpt-5.6-terra` | $2.00 | $12.00 | $0.20 | $2.50 | — |
| OpenAI | `gpt-5.6-sol` | $4.00 | $20.00 | $0.40 | $5.00 | — |
| OpenAI | `gpt-5.6-cyber` | $12.50 | $75.00 | $1.25 | $15.625 | — |
| Anthropic | `claude-sonnet-5` | $2.00 | $10.00 | $0.20 | $2.50 | $4.00 |
| Anthropic | `claude-opus-5` | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| Anthropic | `claude-fable-5` | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |
| Anthropic | `claude-mythos-5` | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |

Aliases include `gpt-5.6 -> gpt-5.6-sol`,
`daybreak-blue-latest -> gpt-5.6-sol`, and
`daybreak-red-latest -> gpt-5.6-cyber`.

OpenAI states that Sol's Standard promotion is available at least through
November 21, 2026. Because it gives neither an exact end date nor replacement
rates, the catalog has no speculative rollback schedule.

Anthropic's current first-party pricing page and release notes state that
Sonnet 5's `$2/$10` rate became the standard rate and the previously planned
September 1 increase will not occur. There is intentionally no scheduled
Sonnet 5 increase in this catalog.

Primary sources:

- [OpenAI API pricing](https://developers.openai.com/api/docs/pricing)
- [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [Anthropic release notes](https://platform.claude.com/docs/en/release-notes/overview)
- [AWS Bedrock pricing](https://aws.amazon.com/bedrock/pricing/)
- [AWS public Bedrock price list](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current/index.json)

## Cost calculation examples

The token portion of a call is:

```text
uncached_input_tokens * input_rate / 1,000,000
+ cache_read_tokens * cache_read_rate / 1,000,000
+ 5m_cache_write_tokens * 5m_write_rate / 1,000,000
+ 1h_cache_write_tokens * 1h_write_rate / 1,000,000
+ output_tokens * output_rate / 1,000,000
+ priced tool/search request fees
```

Provider multipliers are then applied to the dimensions covered by that
provider's contract. An authoritative provider-reported total, such as xAI
cost ticks, wins over catalog reconstruction.

Real-key examples observed during this review:

| Call | Calculation | Cost |
|---|---:|---:|
| GPT-5.6 Luna, 10 input + 4 output | `10*$0.20/M + 4*$1.20/M` | `$0.0000068` |
| GPT-5.6 Terra, 10 input + 4 output | `10*$2/M + 4*$12/M` | `$0.000068` |
| GPT-5.6 Sol, 10 input + 4 output | `10*$4/M + 4*$20/M` | `$0.00012` |
| Sonnet 5 forced tool, buffered, 604 input + 35 output | `604*$2/M + 35*$10/M` | `$0.001558` |

The production organization-counter probe used the Luna call and observed the
same `$0.0000068` increment at project, organization, and model scope.

## Verification ledger

### Final frozen gateway tree

| Gate | Result |
|---|---|
| Rust library tests | **486 passed, 0 failed** |
| NovaGuard platform integration | **61 passed, 0 failed** |
| Policy integration | **12 passed, 0 failed** |
| Clippy, all targets/features, warnings denied | **Passed** |
| Rust formatting and `git diff --check` | **Passed** |
| All-target test compilation | **Passed** |
| `wasm32-unknown-unknown`, no native defaults | **Passed** |
| `worker-build 0.8.5 --release` | **Passed** |
| Wrangler `4.120.0 deploy --dry-run` | **Passed** |
| RustSec audit, with repository allowlist | **Passed** |
| 13-phase pinned Workerd E2E | **Passed** |

The Workerd matrix additionally proved:

- OpenAI and Anthropic buffered/stream settlement;
- Anthropic tools, auth, headers, query and body conversion;
- strict/advisory/shadow/model-scope policy selection;
- transformed-body and tool-schema reservation;
- pre-admission rejection for xAI search, Bedrock Nova, conflicting output
  aliases, Groq Compound, Gemini search/cached context, Mistral documents,
  Together video, media, MCP, Perplexity, OpenRouter, and unpriced custom bases;
- 16-way multi-agent concurrency with no duplicate settlement or leaked hold;
- rate-only opaque-input rejection and `/admit`-503 fail-closed/fail-open parity;
- Worker deadline abandonment before lease expiry, followed by a successful new
  admission with zero reservation reaping;
- request-scoped `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS=128000` parity between
  `/admit` and the Anthropic `max_tokens` forwarded upstream;
- binding-only policy-engine recompilation and monotonic cache replacement, so
  an unchanged ETag or a delayed older refresh cannot restore stale enforcement.

### Companion Noveum app tree

- API unit suite: **738 passed**, 29 integration-mode skips (the 27 Redis
  guardrail cases below were run separately against a real Redis instance).
- Real Redis guardrail integrations: **27/27 passed**.
- Web suite: **391/391 passed**.
- Telemetry suite: **136/136 passed**.
- Focused guardrail/project router suite: **72/72 passed**.
- API, web, and telemetry TypeScript checks: **Passed**.
- Translation parity/usage, schema parity, Biome, comments, and diff checks:
  **Passed**.

The app patch additionally preserves `enforcementMode` and `scopeToModels`
through dashboard load/edit/save, adds a strict/advisory selector and model list,
makes dry-run model-aware, and hardens pricing lookup against inherited object
keys such as `__proto__` and `constructor`.

### Live provider and production checks

Final real-key runs read only the required credential names with a narrow
parser, and no secret value is reproduced here. See the security note below for
the one early diagnostic that did not follow this rule.

- OpenAI GPT-5.6 Luna: buffered and streaming passed.
- Anthropic Sonnet 5: buffered, streaming, buffered tool call, and streaming
  tool call passed with usage/cost parity.
- Groq: buffered and streaming passed.
- Noveum production guard state and guarded usage: passed across two projects.
- Together, Fireworks, and AWS Bedrock did not have usable credentials in the
  available environment, so no claim of paid live coverage is made for them.
- No live Cloudflare deployment was made; Workerd and Wrangler dry-run are the
  deployment evidence for this review.

## Dependency and toolchain decision

The published branch already contains the required transitive security update
from vulnerable `h2 0.4.15` to fixed `h2 0.4.16`; fresh `cargo audit` and
`cargo deny` scans pass. No broader library upgrade is needed. The locked Worker stack is
coherent: `worker`, `worker-macros`, and `worker-sys 0.8.5`,
`wasm-bindgen 0.2.126`, `worker-build 0.8.5`, Wrangler `4.120.0`, and Node 22 in
CI. The final local Workerd run used Node `25.5.0` and Rust `1.96.0` without a
runtime issue. `Cargo.toml` now advertises the actual AWS dependency MSRV,
Rust `1.94.1`.

An old machine-global `worker-build 0.1.2` produced the wrong output layout in
one diagnostic dry-run. Prepending the pinned `0.8.5` binary—the same setup used
by CI and the Workerd harness—made the dry-run pass. Upgrading repository pins
to work around a stale global installation would reduce reproducibility.

## GitHub review state

At the last authoritative refresh:

- Gateway PR #30 was open, non-draft and mergeable, with no approval decision.
- Companion app PR #544 was open, non-draft and mergeable, with no approval
  decision.
- Gateway native, Worker, supply-chain, Docker, and CodeRabbit status checks
  were green on `a0e93f8`. None of those checks covers this final local release
  patch.
- CodeRabbit's summary explicitly said reviews were paused after the commit
  influx; its green check is not a fresh review of this local tree.
- There were 29 review threads: 19 resolved and 10 open.
- Nine open threads were outdated and are addressed by the local implementation.
- The one open/current organization-counter thread can be answered with the
  two-project production evidence after the relevant code is pushed/deployed.

Do not resolve or reply to those threads against the old remote SHA. Push first,
run fresh checks, then reply with the exact test/evidence references.

## Required merge sequence

1. Commit and push both release branches and link PR #30 to companion PR #544.
2. Run all GitHub checks and request fresh review on both pushed SHAs.
3. Merge/deploy the app-side change **before** the gateway. The new gateway
   resolver intentionally fails closed until the companion endpoint exists.
   Confirm
   the production `/state` and `/admit` paths still expose source-keyed
   organization counters and model-scoped operations.
4. Run the provider-smoke workflow and a two-key/same-tenant plus
   two-project/one-organization production probe after the app deploy.
5. Run a live Cloudflare staging deployment if production-edge evidence is a
   release requirement.
6. Request a fresh human and CodeRabbit review.
7. Reply to and resolve the nine stale fixed threads. Close the organization
   thread only with the pushed SHA and production two-project evidence.
8. Approve and merge only after all checks above are green.

## Work deliberately deferred to follow-up PRs

These are documented limitations, not hidden blockers in the strict surface
implemented here:

- strict support for `/v1/responses`, multipart endpoints, audio/files/images,
  provider search, Perplexity/OpenRouter request fees, and arbitrary server
  tools;
- region-aware Bedrock Nova/Titan admission and settlement, including AWS
  partition-specific rate tables;
- Bedrock OpenAI tool-call and multimodal translation;
- exact model tokenizers and image-dimension accounting instead of the current
  conservative serialized-byte bound;
- mixed-model Anthropic fallback settlement;
- Cloudflare shared-tenancy mode (currently refused rather than silently
  mis-enforced);
- native Bedrock temporary-session credential parity;
- a repository-local Node lock for Wrangler and a dedicated minimum-Rust CI
  lane;
- paid live Together, Fireworks, Bedrock, and a live Cloudflare staging matrix.
- exact audit persistence for gateway `pricingVersion` and `costBreakdown`;
- metering ownership for requests spanning advisory/strict policy hot swaps;
- graceful-shutdown draining of detached exporters and shared tenant reporters;
- Worker policy-refresh timeout, singleflight, and monotonic stale-write guard;
- cross-replica atomic admission for native rate-only policies (current native
  rate enforcement remains advisory and may overshoot during refresh windows).

## Security/operations note

During one early read-only environment inspection, a reviewer initially used
shell sourcing on an app `.env`; a malformed/unquoted line caused an opaque
secret-looking value to appear in the internal command trace. The value is not
reproduced here and subsequent access used a narrow parser. If internal command
traces are retained outside the normal trusted development boundary, rotate the
credential associated with that malformed line as a precaution. Never source
the app `.env` in test automation.

## Final decision

**Local corrected implementation:** GO.

**GitHub PR at its currently published SHA:** NO-GO until the local patches and
companion app changes are pushed, deployed in the required order, and pass fresh
CI/review.
