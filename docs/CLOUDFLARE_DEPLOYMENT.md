# Cloudflare deployment architecture

Applies to gateway **v2.0.1** (August 2026). The production evidence in this
document is the dated v2.0.0 baseline; release preparation does not establish
that v2.0.1 has been deployed or promoted.

The repository ships two deployment shapes from the same Rust crate:

| Shape | Runtime | Entry point | Typical use |
|---|---|---|---|
| Cloudflare Worker | Rust compiled to `wasm32-unknown-unknown`, executed by `workerd` | `src/worker_rt.rs` | Globally distributed edge proxy |
| Native binary / Docker | Tokio + Axum on Linux or a host OS | `src/main.rs` | Self-hosting and native shared-tenancy deployments |

The Nova Guard engine, pricing catalog, provider routing, Anthropic translation,
and Bedrock request/response mapping are shared. The HTTP transports, runtime
lifecycle, and supported tenancy modes are runtime-specific, so “one crate” does
not mean every deployment option is identical.

For operator commands and the platform-bridge runbook, use
[CLOUDFLARE_WORKER.md](CLOUDFLARE_WORKER.md).

## Current implementation status

The Worker implementation includes:

- A static, no-JavaScript `GET /` information page with bodyless `HEAD /`
  parity, `/health`, CORS, request-size limits, and `/v1/*` routing. `/docs` and
  `/openapi.json` remain 404.
- Explicitly disabled version-prefix and alias preview URLs. Release candidates
  are attached at zero percent and exercised through Cloudflare's
  version-override header before canary traffic.
- Transparent proxying for the OpenAI-compatible provider routes.
- Anthropic OpenAI Chat Completions ↔ Messages translation for requests,
  successful buffered responses, and successful SSE streams, including function
  tool calls and terminal usage. Cache, `inference_geo`, and `speed` usage
  survive both response paths for settlement. Client function tools are
  supported; native Anthropic server/MCP tool definitions are not. The exact
  subset is documented in [providers/anthropic.md](providers/anthropic.md).
- Buffered Bedrock Converse conversion and SigV4 signing in pure Rust (`sha2` +
  `hmac`), including temporary credentials through `x-aws-session-token` on the
  Worker. Worker `stream: true` is rejected with an OpenAI-shaped HTTP 400
  `unsupported_feature` before admission/AWS; native ConverseStream remains
  supported and now also accepts temporary credentials.
- Inline stateless Nova Guard policies.
- Dedicated platform-managed Nova Guard: effective policies, project **and
  organization** live counters, atomic admission, and reservation settlement.
- Incremental stream metering without buffering. OpenAI-compatible SSE bytes pass
  through; successful Anthropic SSE is translated to OpenAI chunks before it is
  metered and returned.

The v2.0.0 release was deployed to Cloudflare and verified on both
`https://gate.noveum.ai` and its `workers.dev` hostname. The production service
is intentionally **transparent**: it has no project-bound Noveum bridge because
the hostname is shared. Dedicated bridge behavior was validated with isolated
preview versions against the production control plane, including two projects
sharing one organization counter, without assigning that preview project to all
production callers.

Release evidence has five layers:

1. native formatting, lint, build, and hermetic tests;
2. wasm compilation and `worker-build --release`;
3. `wrangler deploy --dry-run`; and
4. `scripts/novaguard_worker_e2e.sh`, a 13-phase suite running the generated
   bundle in real local `workerd` against mock Noveum, OpenAI, and Anthropic
   upstreams; and
5. an immutable Cloudflare preview and production promotion, followed by live
   buffered/SSE checks through OpenAI, Anthropic, and Groq.

The workerd suite covers `worker::Fetch`, policy/state/admission calls,
`ctx.wait_until` settlement, incomplete streams, strict failure behavior,
Anthropic tool-stream translation, mixed concurrent agents, and the matrix that
selects which unbounded requests require a strict output limit. A real edge
smoke remains mandatory for every new deployment; the v2.0.0 result is evidence
for that immutable release, not for future working trees.

## Architecture

```text
client
  |
  v
Cloudflare Worker (workerd / WASM)
  |-- route, authenticate provider request, enforce body limit
  |-- fetch/compile effective policy and read live state (when configured)
  |-- provider preflight and optional atomic admission
  |-- Nova Guard input evaluation/transforms
  |-- provider adapter
  |     |-- OpenAI-compatible: transparent HTTP/SSE proxy
  |     |-- Anthropic: OpenAI request -> Messages; response/SSE -> OpenAI
  |     `-- Bedrock: OpenAI request -> buffered Converse + SigV4; response -> OpenAI
  |-- Nova Guard buffered output phase (streaming output phase is skipped)
  `-- reservation completion/abandon/cancel through ctx.wait_until
```

The Worker needs no provider secret at deployment time: provider credentials are
supplied per request. Platform-managed Nova Guard does require a scoped
`NOVEUM_API_KEY` secret and a fixed `NOVEUM_GUARD_PROJECT_ID`.

## Runtime differences that matter

| Concern | Cloudflare Worker | Native gateway |
|---|---|---|
| Outbound HTTP | `worker::Fetch` | `reqwest` |
| Server runtime | `workerd` isolate | Tokio + Axum |
| Bedrock signing | Pure Rust `sha2` + `hmac`; accepts optional session token | Native AWS SDK signer; accepts optional session token |
| Caller tenancy | Dedicated project only; `NOVEUM_GUARD_TENANCY=shared` is refused with 503 | Dedicated or shared, with tenant derived from `x-noveum-api-key` |
| Local policy source | Inline Worker var/secret; filesystem paths are unavailable | Inline JSON or file |
| Stateful inline policy | Inline `cost_cap` / `rate_limit` is refused with 503 because it has no backend | Without platform state, stateful rules fail open and warn |
| Platform state | Dedicated policy fetch, project/org counters, admission, settlement | Dedicated or per-derived-tenant clients |
| Deployment-wide cost-mode override | Not implemented; each policy's `enforcementMode` decides | `NOVEUM_GUARD_COST_ENFORCEMENT=strict\|advisory` |
| Streaming | OpenAI-compatible pass-through; Anthropic translated incrementally; Bedrock rejected with HTTP 400 `unsupported_feature`; no output-phase Guard enforcement | OpenAI-compatible/Anthropic behavior plus Bedrock ConverseStream translation |
| Telemetry export | No full native exporter; platform settlement is supported | Native telemetry plugins/exporters |

## Nova Guard correctness at the edge

### Organization scope remains organization scope

An organization-sourced `cost_cap` or `rate_limit` reads organization counters
from the control plane's nested organization state. The Worker never substitutes
project counters when organization state is absent. Missing state takes the
policy's unavailable-state path: fail-closed blocks; fail-open allows with an
explicit reason. Substituting project state would let every project spend the
full organization allowance independently.

### Strict cost caps require a real provider bound

For the requested model, a `cost_cap` imposes the explicit-limit requirement
only when all of these are true:

- `mode` is `enforce`;
- `action` is `block`;
- effective `enforcementMode` is `strict`; and
- `scopeToModels` is empty/absent or contains the model (case-insensitive).

Such a request must include a positive `max_tokens`, `max_completion_tokens`, or
`max_output_tokens` value no greater than 10,000,000. Otherwise the Worker
returns HTTP 400 with `error.code: "missing_output_limit"` before `/admit` or the
provider is called. Advisory, shadow, non-blocking, out-of-scope, and rate-only
cases retain the configured assumed-output heuristic where an estimate is
needed.

Strict admission supports bounded JSON `/v1/chat/completions` only. It requires
a non-empty model and messages array, estimates the post-transform serialized
body, and rejects request shapes whose provider-side cost cannot be bounded:
Responses/conversation state, images, files, audio, remote search, server/MCP
tools, multi-choice `n`/`best_of`, Perplexity, and OpenRouter. Direct OpenAI
requests are pinned to `service_tier: "default"`; premium tiers are rejected.
Advisory policies keep the broader transparent-proxy compatibility surface.

Provider preflight also runs before `/admit`. For Anthropic it rejects invalid
cache controls, geo, speed, mixed-model fallbacks, constrained-model sampling,
and Sonnet 5 thinking/prefill. A valid strict reservation includes the entire
estimated prompt at the longest declared cache-write TTL, a conservative 1.1x
premium when eligible geo is omitted (or explicitly US-only), and the 2x Opus
5/4.8 fast-mode premium. Fast plus US geo reserves 2.2x across all token/cache
dimensions. The native gateway uses the same conversion and pricing functions.

### Settlement is conservative

Authoritative provider usage completes a reservation at actual token counts
only when both input and output counts are present; explicit zero is valid.
Converters do not fabricate a missing half as zero. A buffered or streaming
response with missing/partial usage, a truncated stream, a client disconnect,
or a body beyond the inspection limit abandons the reservation and leaves the
estimate in place. A gateway policy block after admission cancels the
reservation because the provider was provably not called. An Anthropic refusal
before any output retains its usage counts but settles monetary cost to $0; a
partial-output refusal is billed normally.

An admitted Worker provider call, buffered body, or stream is bounded to 10
minutes (`NOVEUM_GUARD_WORKER_UPSTREAM_TIMEOUT_MS` may lower but cannot raise
that ceiling). At the deadline the Worker terminates the body and abandons the
reservation conservatively. This guarantees an active Cloudflare stream cannot
outlive the platform's 15-minute pending-reservation lease and have its hold
reaped while the provider is still spending.

## Reproducible toolchain

The repository and CI use a coherent Worker toolchain:

| Tool | Pin |
|---|---|
| `worker`, `worker-macros`, `worker-sys` crates | `0.8.5` in `Cargo.lock` |
| `worker-build` | `0.8.5` |
| Wrangler | `4.120.0` |
| Node.js in CI | `22` |
| Crate MSRV | Rust `1.94.1` |
| Docker builder | Rust `1.96` |
| Worker compatibility date | `2026-08-01` |

Build output is `build/index_bg.wasm` plus the canonical `build/index.js` entry;
`build/worker/shim.mjs` is only a compatibility alias. Use the exact pins rather
than an arbitrary global `worker-build` or Wrangler installation:

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build --version 0.8.5 --locked --force
worker-build --release
npx --yes wrangler@4.120.0 deploy --dry-run
```

The compatibility date and Wrangler version should be reviewed together when
upgrading. A newer package existing upstream is not, by itself, a reason to
change a PR whose pinned build and dry-run pass.

## Release verification

Before deploying a release candidate:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --lib
cargo test --locked --test novaguard_platform
cargo test --locked --test policy_integration
cargo check --locked --target wasm32-unknown-unknown --no-default-features --lib
worker-build --release
npx --yes wrangler@4.120.0 deploy --dry-run
scripts/novaguard_worker_e2e.sh
git diff --check
```

Then follow the explicitly authorized staged deployment and rollback runbook in
[CLOUDFLARE_WORKER.md](CLOUDFLARE_WORKER.md#production-deployment-runbook).
At minimum verify `/health`, one buffered and one streaming OpenAI call, one
buffered and one tool-streaming Anthropic call, an organization cap, a strict
unbounded 400, and reservation settlement. Do not infer a live result from the
hermetic suite.

## Remaining work

Worker-native telemetry, distributed policy storage, Bedrock feature expansion,
and automated authorized smoke coverage are tracked in the [current
roadmap](TODO.md). That roadmap is not a release promise.

## Primary sources

- [Cloudflare Workers Rust support](https://developers.cloudflare.com/workers/languages/rust/)
- [workers-rs v0.8.5 release](https://github.com/cloudflare/workers-rs/releases/tag/v0.8.5)
- [Wrangler 4.120.0 release](https://github.com/cloudflare/workers-sdk/releases/tag/wrangler%404.120.0)
- [Wrangler installation and updates](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- [Worker compatibility dates](https://developers.cloudflare.com/workers/configuration/compatibility-dates/)
- [Workers platform limits](https://developers.cloudflare.com/workers/platform/limits/)
- [Anthropic Messages API](https://platform.claude.com/docs/en/api/messages/create)
- [Anthropic streaming protocol](https://platform.claude.com/docs/en/build-with-claude/streaming)
