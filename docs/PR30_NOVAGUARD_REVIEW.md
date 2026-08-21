# PR #30 NovaGuard release review

**Review date:** 2026-08-21

**Gateway PR:** [Noveum/ai-gateway#30](https://github.com/Noveum/ai-gateway/pull/30)

**Gateway working branch:** `codex/pr30-release-blockers`

**Companion app branch:** `codex/pr30-platform-guardrails`

## Executive verdict

The corrected local implementation is a **GO in the audited code scope**. No
reproducible P0 or P1 correctness blocker remains in the frozen local tree.
NovaGuard works end to end across the native gateway and a real local
Cloudflare Workerd runtime, including atomic admission, strict cost limits,
organization counters, request transforms, provider streaming, usage
settlement, concurrency, and reservation cleanup.

The GitHub PR as currently published is **not yet ready to approve or merge**.
GitHub still points at remote commit `98a562f`, while the release-blocker fixes
and companion app changes described here are local. Approval should happen only
after both repositories are committed/pushed, the app-side changes are
deployed, CI passes on the pushed SHA, and a fresh human/bot review is requested.

Cloudflare compatibility is proven by WASM compilation, pinned
`worker-build 0.8.5`, Wrangler `4.120.0 deploy --dry-run`, and the full ten-phase
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
- Advisory, shadow, non-blocking, rate-only, and model-scope-miss policies retain
  their broader proxy behavior; strict validation is not incorrectly applied
  to them.
- Provider-local deterministic validation and authentication errors happen
  before a reservation is created.
- Unknown providers are rejected before `/state`, `/admit`, or provider fetch.

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

Catalog version `2026.08.21` now covers current model IDs, aliases, cache rates,
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
| OpenAI | `gpt-5.6-sol` | $5.00 | $30.00 | $0.50 | $6.25 | — |
| OpenAI | `gpt-5.6-cyber` | $12.50 | $75.00 | $1.25 | $15.625 | — |
| Anthropic | `claude-sonnet-5` | $2.00 | $10.00 | $0.20 | $2.50 | $4.00 |
| Anthropic | `claude-opus-5` | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| Anthropic | `claude-fable-5` | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |
| Anthropic | `claude-mythos-5` | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |

Aliases include `gpt-5.6 -> gpt-5.6-sol`,
`daybreak-blue-latest -> gpt-5.6-sol`, and
`daybreak-red-latest -> gpt-5.6-cyber`.

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
| GPT-5.6 Sol, 10 input + 4 output | `10*$5/M + 4*$30/M` | `$0.00017` |
| Sonnet 5 forced tool, buffered, 604 input + 35 output | `604*$2/M + 35*$10/M` | `$0.001558` |

The production organization-counter probe used the Luna call and observed the
same `$0.0000068` increment at project, organization, and model scope.

## Verification ledger

### Final frozen gateway tree

| Gate | Result |
|---|---|
| Rust library tests | **478 passed, 0 failed** |
| NovaGuard platform integration | **57 passed, 0 failed** |
| Policy integration | **12 passed, 0 failed** |
| Clippy, all targets/features, warnings denied | **Passed** |
| Rust formatting and `git diff --check` | **Passed** |
| All-target test compilation | **Passed** |
| `wasm32-unknown-unknown`, no native defaults | **Passed** |
| `worker-build 0.8.5 --release` | **Passed** |
| Wrangler `4.120.0 deploy --dry-run` | **Passed** |
| Ten-phase pinned Workerd E2E | **Passed** |

The Workerd matrix additionally proved:

- OpenAI and Anthropic buffered/stream settlement;
- Anthropic tools, auth, headers, query and body conversion;
- strict/advisory/shadow/model-scope policy selection;
- transformed-body and tool-schema reservation;
- pre-admission rejection for xAI search, Bedrock Nova, conflicting output
  aliases, Groq Compound, Gemini search/cached context, Mistral documents,
  Together video, media, MCP, Perplexity, OpenRouter, and unpriced custom bases;
- 16-way multi-agent concurrency with no duplicate settlement or leaked hold;
- Worker deadline abandonment before lease expiry, followed by a successful new
  admission with zero reservation reaping.

Final Workerd artifacts are preserved locally at:

```text
/private/var/folders/7d/c1z608491jq84m9f40phz5rh0000gn/T/tmp.wC71visuv5/
```

### Companion Noveum app tree

Focused Vitest coverage is **40/40 passing**:

- 19 pricing-table tests;
- 15 price-calculation tests;
- 6 model-scope admission tests.

Changed pricing files type-check cleanly and Biome exits zero. Broader app tests
in the isolated worktree are limited by absent generated Prisma/ClickHouse/RBAC
dependencies; the failures occur during dependency collection, not in changed
guardrail or pricing code.

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

No dependency upgrade is required for this PR. The locked Worker stack is
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

- PR #30 was open, non-draft, mergeable/clean, with no approval decision.
- Remote checks were green on `98a562f`, not on this local release patch.
- CodeRabbit's summary explicitly said reviews were paused after the commit
  influx; its green check is not a fresh review of this local tree.
- There were 29 review threads: 19 resolved and 10 open.
- Nine open threads were outdated and are addressed by the local implementation.
- The one open/current organization-counter thread can be answered with the
  two-project production evidence after the relevant code is pushed/deployed.

Do not resolve or reply to those threads against the old remote SHA. Push first,
run fresh checks, then reply with the exact test/evidence references.

## Required merge sequence

1. Commit and push the gateway release-blocker patch.
2. Commit the companion app changes as two reviewable units:
   - guardrail model scoping/admission tests;
   - TypeScript model/pricing parity.
3. Merge/deploy the app-side change before or together with the gateway. Confirm
   the production `/state` and `/admit` paths still expose source-keyed
   organization counters and model-scoped operations.
4. Run native CI, supply-chain/Docker checks, Worker build/dry-run, and the
   provider-smoke workflow on the pushed gateway SHA.
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
