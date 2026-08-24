# Running the Gateway as a Cloudflare Worker (edge)

The **same `noveum-ai-gateway` crate** builds three ways. `wasm32` builds the
Cloudflare Worker; native builds the server (Docker / binary / library). They
share the Nova Guard engine and provider translators. Runtime-specific
transport and tenancy differences are called out below.

| Shape | Build | Entry |
|---|---|---|
| Native binary / Docker | `cargo build --release` | `src/main.rs` (Axum + Tokio) |
| Rust library | `noveum-ai-gateway` dependency (`rlib`) | `src/lib.rs` |
| **Cloudflare Worker** | `worker-build --release` (`cargo … --target wasm32 --no-default-features`) | `src/worker_rt.rs` (`#[event(fetch)]`) |

## Prerequisites (one-time)

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build --version 0.8.5 --locked --force
node --version                       # Wrangler 4.120.0 requires Node >= 22
# (optional, for smaller bundles) install binaryen so `wasm-opt` is on PATH,
# then set `wasm-opt = true` in [package.metadata.wasm-pack.profile.release].
```

CI pins `worker-build` to `0.8.5` and invokes Wrangler as
`npx --yes wrangler@4.120.0`; use the same versions locally. An older global
`worker-build` can emit a different directory layout even when the Rust code is
valid. Cloudflare also recommends a project-local Wrangler installation; this
repository uses the exact `npx` version until it gains an npm lockfile. See
[Cloudflare's Wrangler installation guidance](https://developers.cloudflare.com/workers/wrangler/install-and-update/).

## Build the Worker bundle

```bash
worker-build --release
# → build/index_bg.wasm      (compiled gateway)
#   build/index.js            (canonical JS entry used by wrangler.toml)
#   build/worker/shim.mjs     (backward-compatible re-export alias)
```

## Test locally (no Cloudflare account needed)

`wrangler dev` runs the Worker in `workerd` (the real runtime) on localhost:

```bash
npx --yes wrangler@4.120.0 dev --port 8787
```

Then exercise it exactly like the native gateway. A v2.0.1 source build reports
that version:

```bash
# Health
curl localhost:8787/health
# → {"status":"healthy","version":"2.0.1","runtime":"cloudflare-worker"}

# Proxy an OpenAI-compatible provider (x-provider + Bearer key, OpenAI body)
curl localhost:8787/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" -H "x-provider: openai" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"max_tokens":32}'
```

The checked-in `wrangler.toml` is deliberately transparent: it does not enable
the commented SSN example or any other policy. Real-provider smoke tests require
your own key; CI and the end-to-end test below are hermetic and use no provider
or Noveum credentials.

### Hermetic end-to-end proof of the platform bridge

```bash
scripts/novaguard_worker_e2e.sh          # NODE_BIN_DIR=... if node < 22 is first on PATH
```

This is the only coverage of the three things that exist **only** on `wasm32`
and are therefore invisible to `cargo test`: `worker::Fetch` against the control
plane, `ctx.wait_until` settlement, and the SSE stream tee. Everything else in
`worker_remote.rs` is a pure function with native unit tests; these three were
previously proven by compilation alone.

It needs no Cloudflare account, no Noveum backend and no provider key.
`scripts/novaguard_mock_platform.py` plays the control plane plus the OpenAI and
Anthropic upstreams. The Worker is pointed at it with `NOVEUM_API_URL`,
`OPENAI_BASE_URL`, and `ANTHROPIC_BASE_URL`. The 13 phases start fresh isolates
whenever policy changes require them (a restart is how the 60-second in-isolate
policy cache is dropped):

| Phase | Proves |
|---|---|
| 1. Allowed | `/effective` + `/state` + `/admit` over `worker::Fetch`; `stream_options.include_usage` forced onto the upstream request; the SSE stream reaching the client intact; and `ctx.wait_until` settling **after** the body completes, at the stream's real token counts rather than the estimate |
| 2. No usage frame | The reservation **abandons** with a distinguishing reason and the conservative estimate stays applied, rather than completing at a fabricated zero |
| 3. `/admit` 503 | A fail-closed strict cap **blocks** and the provider is never called: a 503 is unevaluable, never an implicit allow |
| 4. Over the cap | A platform block arrives as **HTTP 200 `{allowed:false}`**, the reason reaches the client, and the provider is never called |
| 5. Strict unbounded request | An enforcing/blocking strict cap returns HTTP 400 `missing_output_limit` before admission or provider dispatch |
| 6. Anthropic translation | OpenAI Chat Completions input becomes a native Messages request; tool-call SSE, fragmented JSON, finish reason, usage, and `[DONE]` return in OpenAI shape without raw Anthropic events |
| 7. Guarded Anthropic + pricing parity | Translated terminal usage settles the platform reservation at authoritative counts; strict admission also prices Sonnet 5's 1-hour cache-write premium, omitted-geo 1.1x reservation, and the active Opus 5 row rather than a token-only approximation |
| 8. Concurrent agents | 16 mixed OpenAI/Anthropic, buffered/streaming callers overlap by default (`CONCURRENT_CLIENTS` may be 12, 16, or 20); each gets one admission and exactly one completion, with no leaked active hold |
| 9. Strict-policy selection and bounded input | A real workerd matrix proves only enforcing, blocking, in-scope strict caps require an output bound; advisory, shadow, and out-of-scope cases remain allowed. Admission measures the exact transformed upstream JSON, adds the separate 4,096-token reserve only for client tools, and rejects 15 opaque or premium strict-input shapes before `/admit` or the provider |
| 10. Rate-only strict input | Opaque request bodies are rejected before admission/provider dispatch, and admission outages independently honor the rate policy's `failClosed` setting |
| 11. Reservation lease safety | A deliberately slow SSE call is terminated and abandoned before a shortened test lease can expire; a follow-up request receives a new reservation, while the live first reservation is never reaped or double-spent |
| 12. Worker output-token binding | `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS=128000` drives both `/admit.maximumOutputTokens` and an unbounded Anthropic request's upstream `max_tokens` |
| 13. Policy refresh ordering | A delayed older refresh cannot replace the blocking policy installed by a faster newer refresh in the same isolate |

Phase 1 is the one worth reading the output of. A `gpt-4o` request with
`max_tokens: 4096` reserves `$0.040965` up front and settles at `$0.0000675` —
the tee recovered 11 input / 4 output tokens from the terminal usage frame, so
the hold is reconciled down by ~600x. Without the tee, every streamed request on
the edge would bill at its ceiling.

## Production deployment runbook

Cloudflare separates a Worker **version** (code, bindings, and compatibility
settings) from a **deployment** (the version or traffic split serving routes).
Upload and test an immutable candidate before assigning production traffic.

The v2.0.0 release was validated on `gate.noveum.ai` and its `workers.dev`
hostname with buffered and SSE calls through OpenAI, Anthropic, and Groq.
Production was deliberately deployed in transparent mode. This is evidence for
that release, not permission to skip the following checks for a new version.

### 1. Authenticate, build, and validate

```bash
npx --yes wrangler@4.120.0 login
npx --yes wrangler@4.120.0 whoami
worker-build --release
npx --yes wrangler@4.120.0 deploy --dry-run
scripts/novaguard_worker_e2e.sh
```

Provider keys are passed per request and must not be Worker secrets. For a
transparent shared hostname, confirm `wrangler.toml` has no Noveum project var
and `wrangler secret list` has no `NOVEUM_API_KEY`. For a dedicated guarded
Worker, configure the scoped bridge secret described below and verify that its
fixed project is the only tenant this domain serves.

### 2. Upload without production traffic

```bash
npx --yes wrangler@4.120.0 versions upload \
  --tag v2.0.1-candidate \
  --message "Noveum AI Gateway v2.0.1 candidate"
npx --yes wrangler@4.120.0 versions list --json
```

Record the candidate version ID, export it in the operator shell, and inspect
it before creating a deployment:

```bash
: "${CANDIDATE_VERSION_ID:?export the uploaded candidate version ID}"
npx --yes wrangler@4.120.0 versions view "$CANDIDATE_VERSION_ID"
```

The checked-in `preview_urls = false` deliberately
prevents [version-prefix and alias hostnames](https://developers.cloudflare.com/workers/versions-and-deployments/preview-urls/)
from becoming public. Do not turn it on ad hoc for a production service; target
the candidate through a zero-percent deployment and version override in the
next step. Preview URLs that are intentionally enabled for another Worker must
be protected with Cloudflare Access and removed after their bounded test
campaign.

### 3. Promote and observe

First record the active deployment and its previous stable version. Attach the
candidate at zero percent, then use Cloudflare's
[version-override header](https://developers.cloudflare.com/workers/versions-and-deployments/version-overrides/)
to exercise that exact version through the real custom domain without routing
ordinary traffic to it:

```bash
: "${PREVIOUS_VERSION_ID:?export the recorded stable version ID}"
: "${CANDIDATE_VERSION_ID:?export the uploaded candidate version ID}"
npx --yes wrangler@4.120.0 deployments list
npx --yes wrangler@4.120.0 versions deploy \
  "${PREVIOUS_VERSION_ID}@100%" "${CANDIDATE_VERSION_ID}@0%" \
  --message "Attach v2.0.1 for targeted smoke" -y

curl --fail --silent https://gate.noveum.ai/health \
  -H "Cloudflare-Workers-Version-Overrides: noveum-ai-gateway=\"${CANDIDATE_VERSION_ID}\""
```

With the same override header, verify `GET /`, bodyless `HEAD /`, `/health`, a
bounded buffered request, and a bounded SSE request. Beginning with v2.0.1,
`/` is a static no-JavaScript information page; `/docs` and `/openapi.json` are not
API-documentation routes and remain 404. Confirm in logs that the override was
applied; an invalid or not-yet-propagated override otherwise follows the normal
traffic split.

For a guarded candidate, also verify policy fetch/state, a successful strict
admission, actual-usage settlement, a strict unbounded 400, organization scope,
and zero leftover active test reservations. Use low-budget test credentials and
remove temporary policies when finished.

After the exact-version smoke passes, assign explicit canary percentages,
observe, then promote the candidate to 100%:

```bash
: "${PREVIOUS_VERSION_ID:?export the recorded stable version ID}"
: "${CANDIDATE_VERSION_ID:?export the uploaded candidate version ID}"
npx --yes wrangler@4.120.0 versions deploy \
  "${PREVIOUS_VERSION_ID}@90%" "${CANDIDATE_VERSION_ID}@10%" \
  --message "Canary v2.0.1" -y

npx --yes wrangler@4.120.0 versions deploy \
  "${CANDIDATE_VERSION_ID}@100%" \
  --message "Promote v2.0.1" -y
```

Check both the custom domain and `workers.dev` hostname. `/health` must report
the promoted package version. Repeat buffered/SSE provider probes through the
real production route and watch errors during the observation window:

```bash
curl --fail --silent https://gate.noveum.ai/health
npx --yes wrangler@4.120.0 tail --status error
```

### 4. Roll back

Rollback creates a new deployment immediately across all routes and domains.
Use the exact stable version recorded before promotion:

```bash
: "${PREVIOUS_VERSION_ID:?export the recorded stable version ID}"
npx --yes wrangler@4.120.0 rollback "$PREVIOUS_VERSION_ID" \
  --message "Rollback failed gateway deployment"
```

After rollback, verify both health endpoints, repeat one buffered and one SSE
call, and inspect errors. Do not delete the stable rollback version during
candidate-version cleanup.

### Observability

`wrangler tail` provides real-time logs. For persistent Workers Logs, explicitly
enable observability in `wrangler.toml`, choose a sampling rate appropriate for
traffic and data sensitivity, and redeploy:

```toml
[observability]
enabled = true
head_sampling_rate = 0.1
```

Worker versions include configuration, so verify observability and bindings on
the exact candidate. Alert on Worker exceptions, sustained 5xx, fail-closed
admission outages, settlement retry exhaustion, and unexpected version skew.
Cloudflare's [Workers Logs documentation](https://developers.cloudflare.com/workers/observability/logs/workers-logs/)
describes retention and export options.

### Configuration (Worker `[vars]` / secrets)
- The checked-in default is a **transparent proxy** — no policies, so nothing is
  mutated or blocked until you opt in.
- `preview_urls = false` — version-prefix and alias preview hostnames remain
  disabled even though the production `workers.dev` route is enabled. Candidate
  smoke tests use a zero-percent deployment plus the version-override header.
- `NOVEUM_GUARD_POLICIES` — inline `nova-guard.json` (same schema as the native
  `NOVEUM_GUARD_POLICIES`/file). Setting it activates stateless Nova Guard.
  Workers KV policy loading is not implemented in this release; use a Worker
  secret if the bundle must not be committed.
- `NOVEUM_GUARD_ENABLED` — `true`/`false` (default `true`; with no policies it's
  still a no-op).
- `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` — `synthetic_success` (default: HTTP 200 with
  a refusal-style completion) or `provider_error` (HTTP 403 + error envelope).
  Identical to the native server.
- `NOVEUM_API_KEY` (**secret**) + `NOVEUM_GUARD_PROJECT_ID` — enable
  platform-managed Nova Guard; see the next section. Optional companions:
  `NOVEUM_API_URL`, `NOVEUM_GUARD_ALLOW_UNGUARDED_START`.
- `NOVEUM_GUARD_TENANCY` — `dedicated` only. `shared` is native-only and is
  **refused** with a 503 rather than ignored; see the table below.
- `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` — fallback estimate for advisory cost
  caps and rate-only admission (default `1024`). It never makes an unbounded
  strict cost cap safe.
- `NOVEUM_GUARD_COST_ENFORCEMENT` — **native gateway only**. The Worker uses each
  policy's `enforcementMode`; do not set this expecting a deployment-wide Worker
  override.
- `NOVEUM_GUARD_WORKER_UPSTREAM_TIMEOUT_MS` — deadline for an admitted provider
  fetch, buffered body, or stream. The default and hard maximum are 600,000 ms
  (10 minutes); operators may set a smaller positive value. At the deadline the
  Worker returns/terminates with a timeout and abandons the reservation, keeping
  its conservative estimate. This occurs before the platform's 15-minute active
  reservation lease can be reaped while a Cloudflare stream is still connected.
- `OPENAI_BASE_URL` — send `x-provider: openai` traffic to a compatible upstream
  instead of `api.openai.com`. The same override the native gateway honors, and
  normalized by the same rule (trim and drop trailing slashes); empty or
  whitespace-only values are treated as unset and use the canonical default. It
  is what lets the Worker be tested against a local mock; in production leave
  it unset.
- `ANTHROPIC_BASE_URL` — override the Anthropic upstream on the same terms; the
  Worker still appends `/v1/messages`. Leave unset in production unless the
  target is an Anthropic-compatible proxy.

Non-JSON `/v1/*` request bodies (multipart, binary) are forwarded byte-for-byte
and are not inspected; only `application/json` bodies run through Nova Guard.
Request bodies over **8 MiB** are rejected with `413`.

## Platform-managed Nova Guard on the Worker

The Worker runs Nova Guard in **both** shapes:

* **Inline / stateless** — a `NOVEUM_GUARD_POLICIES` bundle of text policies
  (`regex_match`, `pii_detection`, secrets, banned terms, model allowlist, JSON
  schema, token limits), decided entirely from the payload in front of the
  gateway using the shared deterministic policy engine.
* **Platform-managed** — set `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` and
  the Worker fetches the project's *effective* policy set, reads the live
  cost/rate counters, and reserves every request against the platform's
  **atomic admission API**. An enforcing/blocking strict `cost_cap` therefore
  holds across Worker isolates rather than per isolate. Advisory, shadow, and
  model-scoped-away policies keep their own semantics.
  Implemented in `src/policy/worker_remote.rs` on `worker::Fetch` — no
  `reqwest`, no Durable Object, no Cloudflare binding.

### The request lifecycle

| Step | Call | Notes |
|---|---|---|
| 1. Policies | `GET /api/v1/projects/{id}/policies/effective` | Cached per isolate (~60 s) and revalidated with `If-None-Match`, so an unchanged set costs a `304`. A failed **refresh** keeps the last known set; a failed **first** fetch refuses the request (see the runbook). |
| 2. Live state | `GET .../policies/state` | Cached ~10 s. Unavailable state is *unavailable*, never "zero spend": `failClosed` policies block, fail-open policies allow with the reason recorded. |
| 3. Provider + bounded-input/output preflight | in-process | Anthropic cost-affecting fields are translated and validated first: cache controls, geo, speed, mixed-model fallbacks, constrained-family sampling, and Sonnet 5 thinking/prefill fail with HTTP 400 before a reservation. Then, for this model, an enforcing/blocking strict `cost_cap` requires bounded JSON `/v1/chat/completions` with a positive `max_tokens`, `max_completion_tokens`, or `max_output_tokens` (all supplied aliases must agree; maximum 10,000,000). Compatible routes receive one normalized upstream ceiling. Missing → HTTP 400 `missing_output_limit`. Responses/conversation state, images, files, audio, remote search (including xAI `search_parameters`), server/MCP tools, multi-choice requests, Perplexity, OpenRouter, and premium service tiers are rejected under strict admission. Direct OpenAI is pinned to `service_tier: "default"`. Advisory, shadow, out-of-scope, and rate-only cases retain the broader request surface and do not acquire the output-bound requirement. |
| 4. Admission | `POST .../policies/admit` | Only when the set contains `cost_cap`/`rate_limit`. Atomically reserves estimated input, maximum output, and request-declared billable dimensions. Anthropic cache-write TTL, geo, and supported fast-mode premiums are therefore present in `estimatedCostUsd`; fast plus US geo reserves 2.2x token/cache rates. **A block is HTTP 200** with `allowed:false`; a **503 is never an allow**. 2 s budget, one retry on a 5xx/429 (the same `requestId` replays, so it cannot double-reserve). |
| 5. Enforcement | in-process | The shared engine runs the input phase against the live counters, then the output phase — identical policy code to the native server. |
| 6. Settlement | `POST .../policies/reservations/{id}/{complete\|abandon\|cancel}` | Scheduled on `ctx.wait_until(...)` **before** the response is returned, so it never blocks the client and never floats. Admitted upstream work has a hard 10-minute maximum, ensuring settlement or conservative abandonment before the platform's 15-minute lease can expire. |

### Settlement semantics (identical to native)

| Outcome | Endpoint | Effect |
|---|---|---|
| Buffered response or stream reported both authoritative input/output counts (including explicit output `0`) | `complete` | Reconciles the hold **down** from `input + max_tokens` to what actually ran. |
| Anthropic pre-output refusal with zero output tokens | `complete` | Retains token/cache counts for observability but settles monetary cost to **$0**. A partial-output refusal is billed normally. |
| Stream ended with no usage frame, provider truncated, input-only report (Anthropic `message_start` then death), body past the 8 MiB inspection cap, non-JSON body | `abandon` | The conservative estimate **stays applied** — the call may have reached the provider. |
| A gateway-side policy blocked the request after admission | `cancel` | Releases the hold. This is the only place `cancel` is used: the call provably never reached the provider. |
| Client disconnected mid-stream | `abandon` | The teed body drops its sender, the settlement future resolves, the estimate stands. |
| Worker upstream deadline reached | `abandon` | The provider body/stream is terminated within 10 minutes and the estimate stands, preventing a still-live request from outlasting and being reaped from the platform's 15-minute reservation lease. |

**Streaming usage is recovered without buffering.** For OpenAI-compatible
streams, each upstream chunk is fed to the shared `StreamUsageScanner`
(`src/policy/metering.rs`) and yielded unchanged, in order, with backpressure.
For a successful Anthropic stream, the shared state machine first converts
Messages events into OpenAI `chat.completion.chunk` frames (including text,
function tool-call fragments, finish reason, terminal usage, and `[DONE]`); the
translated stream is then teed for settlement. Anthropic bytes are therefore
**not** byte-for-byte pass-through. While the bridge is metering,
`stream_options.include_usage` is forced on literal `x-provider: openai`
streaming requests — without it a stream carries no counts and keeps its
conservative reservation estimate.

### Configuration outcomes

| Configuration | Worker behavior |
|---|---|
| `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` set | **Platform-managed Nova Guard**, in dedicated mode. Policies, live state and admission come from the control plane; any inline `NOVEUM_GUARD_POLICIES` is ignored (with a warning) — the platform is the source of truth. |
| `x-provider: bedrock` with `stream: true` | **400 `unsupported_feature` before admission or AWS dispatch.** The v2.0.1 Worker supports buffered Converse only; use the native gateway for ConverseStream. |
| Applicable `mode: enforce`, `action: block`, `enforcementMode: strict` cost cap, but no positive explicit output limit | **400 `missing_output_limit`.** The Worker does not call `/admit` or the provider. Add `max_tokens`, `max_completion_tokens`, or `max_output_tokens`. |
| Strict request supplies conflicting non-null output-limit aliases | **400 `unsupported_strict_input`.** The Worker does not reserve or forward a request whose admitted and upstream ceilings could differ. |
| Advisory, shadow, or model-scoped-away cost cap; or `rate_limit` only | No strict output-limit rejection. When an estimate is needed, the Worker uses `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS`. Cost/rate policy actions are `block`; use shadow mode or `softUsd` for nonblocking observation. |
| `NOVEUM_GUARD_TENANCY=shared` | **503 `gateway_configuration_error`.** Shared tenancy derives each caller's project and organization from its own Noveum credential, which needs the native gateway's tenancy layer. The Worker refuses rather than ignoring the variable: unread, it would proxy every caller unguarded on a deployment its operator believes enforces per-tenant caps. Use the native gateway, or `NOVEUM_GUARD_TENANCY=dedicated`. |
| `NOVEUM_GUARD_TENANCY` set to anything but `dedicated`/`shared`, or to `""` | **503.** A typo or an unresolved template must not fall through to dedicated. |
| Only *one* of the pair set, or either set to `""` | **503 `gateway_configuration_error`.** A half-applied bridge is still an attempt to enable enforcement; falling through to an unguarded proxy would reward the mistake with a 200. Same matrix as the native `RemoteConfig::from_values`. |
| Bridge set, but the **first** policy fetch fails | **503.** No policy set is known, so every request would be forwarded unguarded. Override with `NOVEUM_GUARD_ALLOW_UNGUARDED_START=true` (emergency only). |
| Bridge set, admission returns **503**/times out | `failClosed` decides: a fail-closed strict cap **blocks**; otherwise the request proceeds and the outage is logged. Never an implicit allow. |
| `NOVEUM_GUARD_POLICIES` containing `cost_cap` or `rate_limit`, **no** bridge | **503.** An inline bundle has no live spend/rate backend, so those policies could only evaluate to "allow" and their `failClosed` flag would be neutralized. Configure the bridge instead. |
| `NOVEUM_GUARD_POLICIES` set but not valid JSON / not a valid bundle | **503**, plus a structured `console_error`. Parsing the error away would silently drop every policy the operator deployed. |
| `NOVEUM_GUARD_POLICIES` unset (or empty), no bridge | Transparent proxy — the checked-in default. |
| `NOVEUM_GUARD_POLICIES` with text policies only | Enforced, identical to native. |

“Model-scoped-away” in this table assumes a valid JSON body carrying a model.
When any enforcing/blocking strict cap exists, a non-JSON or multipart request
cannot prove its scope and receives HTTP 400 before admission.

Request bodies are capped at **8 MiB**, the same bound as response inspection.
The declared `Content-Length` is checked before a byte is read, and the cap is
re-enforced while reading so a chunked body with no (or a lying) length cannot
allocate the isolate to death. Over the cap → **413**.

### Operational runbook

**Enable the bridge**

Do not use `wrangler secret put` for a production bridge change: that command
creates a version and immediately deploys it. A key-only or project-only version
is intentionally rejected by `/v1/*`, so sequential immediate mutations cause
an avoidable outage. Instead, have the approved secret manager materialize a
mode-`0600` JSON file outside the repository with both bindings:

```json
{
  "NOVEUM_API_KEY": "replace-with-scoped-service-key",
  "NOVEUM_GUARD_PROJECT_ID": "replace-with-dedicated-project-id"
}
```

Create one **undeployed** version containing both secrets. Passing the project
ID as a secret is allowed and makes the binding change atomic even though that
identifier is not itself confidential:

```bash
: "${NOVEUM_BRIDGE_SECRETS_FILE:?export the secure JSON file path}"
npx --yes wrangler@4.120.0 versions secret bulk \
  "$NOVEUM_BRIDGE_SECRETS_FILE" \
  --tag novaguard-dedicated-enable \
  --message "Stage dedicated Nova Guard bridge"
npx --yes wrangler@4.120.0 versions list --json
```

`NOVEUM_API_KEY` must be a **secret**, never a `[vars]` entry: `[vars]` is
committed to `wrangler.toml` and readable in the dashboard. Both values are
read with `env.secret()` first and `env.var()` as a fallback, so using a secret
for the project ID preserves the same runtime semantics. Record the new version
as `CANDIDATE_VERSION_ID`, then follow the zero-percent override, guarded smoke,
canary, and promotion process in [Promote and observe](#3-promote-and-observe).
Only the final `versions deploy` step changes production traffic. Remove the
local secrets file according to the secret manager's cleanup policy after the
version is verified.

Cloudflare documents that the ordinary secret commands deploy immediately,
whereas the `versions secret` commands only create a version for later
promotion; see [Workers secrets](https://developers.cloudflare.com/workers/configuration/secrets/).

**Verify it is live**

```bash
curl -i $GW/v1/chat/completions -H "x-provider: openai" \
  -H "Authorization: Bearer $OPENAI_API_KEY" -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"max_tokens":16}'
```

Then check the project's usage in the Noveum dashboard: a completed request
appears with real token counts (not the `max_tokens` estimate).
`npx --yes wrangler@4.120.0 tail` streams the Worker's `console_*` lines, which
name every admission block, fail-closed decision, and abandoned settlement.

**When something is wrong**

| Symptom | Cause | Action |
|---|---|---|
| Every `/v1/*` returns 503 `gateway_configuration_error` | Half-applied credentials, a blank secret, an inline `cost_cap`, or a malformed bundle — the message says which | Fix the named variable and redeploy. |
| 503 naming "policy set could not be fetched" | The control plane is unreachable or the key is rejected | Check the key's scopes and `NOVEUM_API_URL`. If an audited emergency decision permits unguarded traffic, stage `NOVEUM_GUARD_ALLOW_UNGUARDED_START=true` with `versions secret put` and the versioned promotion flow. The ordinary `secret put` command deploys immediately and is break-glass only. Caps are **not** enforced while the flag is set; stage its removal as soon as the fetch recovers. |
| 400 with `error.code: "missing_output_limit"` | This model is covered by an enforcing/blocking strict cost cap and the request is unbounded | Send a positive `max_tokens`, `max_completion_tokens`, or `max_output_tokens`. Do not raise the assumed-output heuristic: strict mode deliberately refuses a heuristic bound. |
| 400 `invalid_value` naming an output-limit field | The selected limit is zero, negative, non-integer, or above 10,000,000 | Correct the named field. The request was rejected before admission and provider dispatch. |
| Anthropic 400 `invalid_request_error` naming `fallbacks`, `speed`, `inference_geo`, sampling, `cache_control`, thinking, or assistant prefill | The OpenAI-to-Messages adapter rejected an unsupported or unmeterable request shape | Follow the exact matrix in [the Anthropic provider guide](providers/anthropic.md). The Worker has not admitted the request or called Anthropic. |
| Requests blocked with "platform admission unavailable … failing closed" | Admission returned 503 / exceeded its 2 s budget while a fail-closed strict cap is active | This is the policy working as written. Investigate the platform; setting the cap to fail-open trades enforcement for availability. |
| OpenAI streaming spend remains near the reservation ceiling | `stream_options.include_usage` was stripped by something between the Worker and OpenAI, so the stream kept its estimate | Check `wrangler tail` for `abandon` settlement; Together and Fireworks use their provider-emitted terminal chunks instead of this option. |
| `reservation … gave up after 3 attempts` in the logs | Settlement could not reach the platform | The hold expires server-side at `expiresAt` — conservatively, i.e. still counted until then. No action beyond fixing connectivity. |

**Remove the bridge atomically**

For the version-secret layout above, have the secret manager create a protected
JSON file whose explicit `null` values delete both bindings. Omitted secrets are
retained, so both names must be present:

```json
{
  "NOVEUM_API_KEY": null,
  "NOVEUM_GUARD_PROJECT_ID": null
}
```

Wrangler 4.120.0 supports JSON `null` deletion. Create one undeployed removal
version, record its ID, and use the same zero-percent override and promotion
flow. The targeted smoke must show transparent provider routing and no Noveum
control-plane calls before promotion:

```bash
: "${NOVEUM_BRIDGE_REMOVAL_FILE:?export the protected JSON file path}"
npx --yes wrangler@4.120.0 versions secret bulk \
  "$NOVEUM_BRIDGE_REMOVAL_FILE" \
  --tag novaguard-dedicated-remove \
  --message "Stage dedicated Nova Guard bridge removal"
npx --yes wrangler@4.120.0 versions list --json
```

If a legacy deployment stores `NOVEUM_GUARD_PROJECT_ID` in `[vars]`, remove that
entry in the same reviewed version that deletes the API-key secret; do not use
sequential `wrangler secret delete` plus `wrangler deploy` commands. Verify the
candidate contains neither binding before assigning traffic. At no point should
a half-applied bridge version receive production traffic.

## Implemented edge behavior and verification scope

- `/health`.
- **OpenAI-compatible providers**: `openai, groq, together, fireworks, mistral,
  cohere, google/gemini, deepseek, xai/grok, openrouter, perplexity` use the
  transparent proxy path.
- **Anthropic**: OpenAI Chat Completions requests are normalized and sent to
  `/v1/messages`; either `Authorization: Bearer` or `x-api-key` is accepted,
  `anthropic-beta` is forwarded, and successful buffered **and SSE/tool**
  responses are translated back to OpenAI shape. Both response paths preserve
  cache, `inference_geo`, and `speed` usage for settlement. Standard speed is
  supported; fast mode is limited to Opus 5/4.8 and is priced at 2x, stacking
  with the 1.1x US-geo premium. Client function tools are supported; native
  Anthropic server/MCP tool definitions are not. See the exact supported-field
  and error contract in [the Anthropic provider guide](providers/anthropic.md).
- **Bedrock**: OpenAI → Bedrock **Converse** request, **AWS SigV4**-signed in pure
  Rust (`sha2`+`hmac`; see `src/sigv4.rs`) with credentials from `x-aws-*` headers,
  and the Converse response converted back to OpenAI shape. Like native, the
  edge accepts **temporary credentials** via `x-aws-session-token`. Pass
  `x-aws-access-key-id`, `x-aws-secret-access-key`, `x-aws-region` (+ optional
  `x-aws-session-token`) instead of `Authorization`:

  ```bash
  curl $GW/v1/chat/completions -H "x-provider: bedrock" \
    -H "x-aws-access-key-id: $AWS_ACCESS_KEY_ID" \
    -H "x-aws-secret-access-key: $AWS_SECRET_ACCESS_KEY" \
    -H "x-aws-session-token: $AWS_SESSION_TOKEN" \
    -H "x-aws-region: us-east-1" -H "Content-Type: application/json" \
    -d '{"model":"amazon.nova-micro-v1:0","messages":[{"role":"user","content":"hi"}],"max_tokens":50}'
  ```
  This Worker path is buffered only. `stream: true` returns an OpenAI-shaped
  HTTP 400 with `error.code: "unsupported_feature"` before a Nova Guard
  reservation or AWS request. The native gateway implements Bedrock
  ConverseStream.
- **SSE streaming**: OpenAI-compatible streams pass through unbuffered.
  Successful Anthropic streams are translated incrementally into OpenAI chunks;
  text, function tool calls, finish reason, terminal usage, and `[DONE]` are
  preserved at the protocol level, but the bytes necessarily change. Input
  policies still apply; output-phase enforcement is skipped on streams in this
  release. When the platform bridge holds a reservation, the client-facing
  stream is teed without buffering so authoritative terminal usage can settle
  it.
- **Header + query pass-through**: client request headers (e.g. `OpenAI-Beta`,
  `OpenAI-Organization`, `anthropic-beta`) and the query string are forwarded;
  upstream response headers (`x-request-id`, rate-limit, …) are preserved. Only
  the transparent OpenAI-compatible path can forward a body byte-for-byte;
  Anthropic and Bedrock requests are translated and re-serialized, as are bodies
  changed by Nova Guard.
- **CORS**: permissive (`*`) on all responses + `OPTIONS` preflight, matching the
  native `CorsLayer`.
- **Nova Guard — input phase**: block + redact/mask (regex, PII, secrets, banned
  substrings, model allowlist, JSON-schema, token caps — the full shared engine).
  Input redactions are applied to the body *before* the upstream call.
- **Nova Guard — output phase**: block + redact on non-streaming responses
  buffered up to an 8 MiB inspection cap. A declared larger body passes through
  uninspected; a chunked/undeclared body that crosses the cap while being read
  returns 502 because the original stream can no longer be replayed.
- **Platform-managed Nova Guard**: effective-policy fetch, live cost/rate state,
  atomic admission and reservation settlement — including strict unbounded
  rejection and the policy-selection matrix described above.
  Strict Bedrock admission is limited to catalogued commercial Claude
  model/profile IDs with a derivable global/geographic rate; source-region-priced
  Nova/Titan requests are rejected before `/admit` and remain available under
  advisory policies.

The same Nova Guard policy schema, decisions, and redactions run here as on the
native server (the engine + request/response shaping are one shared codebase).
Note the redact replacement key is **`redactWith`** (see `wrangler.toml`).

The v2.0.0 release evidence includes wasm compilation, `worker-build --release`,
`wrangler deploy --dry-run`, native unit/integration tests, the 13-phase
hermetic `workerd` suite, dedicated guarded previews against the production
control plane, and a transparent production deployment with live OpenAI,
Anthropic, and Groq buffered/SSE probes. The hermetic suite exercises actual
`worker::Fetch`, Anthropic translation, `ctx.wait_until`, concurrent agents,
stream settlement, and strict-policy selection against mocks. Future working
trees need their own edge evidence.

Known follow-up work is centralized in the [current roadmap](TODO.md); it is not
a release promise.
