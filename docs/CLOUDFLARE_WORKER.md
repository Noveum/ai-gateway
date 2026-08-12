# Running the Gateway as a Cloudflare Worker (edge)

The **same `noveum-ai-gateway` crate** builds three ways. `wasm32` builds the
Cloudflare Worker; native builds the server (Docker / binary / library). They
share the Nova Guard engine + provider routing, so behavior is identical.

| Shape | Build | Entry |
|---|---|---|
| Native binary / Docker | `cargo build --release` | `src/main.rs` (Axum + Tokio) |
| Rust library | `noveum-ai-gateway` dep (rlib) | `lib.rs` |
| **Cloudflare Worker** | `worker-build --release` (`cargo … --target wasm32 --no-default-features`) | `src/worker_rt.rs` (`#[event(fetch)]`) |

## Prerequisites (one-time)

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build           # Rust→WASM bundler for Workers
npm i -g wrangler                    # or use `npx wrangler`
# (optional, for smaller bundles) install binaryen so `wasm-opt` is on PATH,
# then set `wasm-opt = true` in [package.metadata.wasm-pack.profile.release].
```

## Build the Worker bundle

```bash
worker-build --release
# → build/worker/index.wasm  (the compiled gateway)
#   build/worker/shim.mjs     (the JS entry wrangler serves)
```

## Test locally (no Cloudflare account needed)

`wrangler dev` runs the Worker in `workerd` (the real runtime) on localhost:

```bash
npx wrangler dev --port 8787
```

Then exercise it exactly like the native gateway:

```bash
# Health
curl localhost:8787/health
# → {"status":"healthy","version":"1.2.0","runtime":"cloudflare-worker"}

# Proxy an OpenAI-compatible provider (x-provider + Bearer key, OpenAI body)
curl localhost:8787/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" -H "x-provider: openai" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}'

# Nova Guard block (the default wrangler.toml sets a block-SSN policy)
curl -i localhost:8787/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" -H "x-provider: openai" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"ssn 123-45-6789"}]}'
# → HTTP 200 with header `x-noveum-guard-blocked: true`
```

Verified locally: `/health`, OpenAI + Groq proxy (real upstreams), and the SSN
block all behave identically to the native server.

## Deploy to the global edge

```bash
npx wrangler login                       # authenticate to your Cloudflare account
# Provider keys are passed PER REQUEST (Authorization header), so the Worker
# itself needs no provider secrets. Set Nova Guard policy inline or via KV:
#   - inline: edit NOVEUM_GUARD_POLICIES in wrangler.toml ([vars])
#   - or a secret:  npx wrangler secret put NOVEUM_GUARD_POLICIES
npx wrangler deploy                      # publishes to 330+ PoPs
```

After deploy, the gateway answers at `https://noveum-ai-gateway.<account>.workers.dev`
(or a custom route/domain) from the nearest PoP to each caller.

### Configuration (Worker `[vars]` / secrets)
- The checked-in default is a **transparent proxy** — no policies, so nothing is
  mutated or blocked until you opt in.
- `NOVEUM_GUARD_POLICIES` — inline `nova-guard.json` (same schema as the native
  `NOVEUM_GUARD_POLICIES`/file). Setting it activates Nova Guard. For production,
  prefer a **Workers KV** binding so policies propagate globally without a redeploy.
- `NOVEUM_GUARD_ENABLED` — `true`/`false` (default `true`; with no policies it's
  still a no-op).
- `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` — `synthetic_success` (default: HTTP 200 with
  a refusal-style completion) or `provider_error` (HTTP 403 + error envelope).
  Identical to the native server.
- `NOVEUM_API_KEY` (**secret**) + `NOVEUM_GUARD_PROJECT_ID` — enable
  platform-managed Nova Guard; see the next section. Optional companions:
  `NOVEUM_API_URL`, `NOVEUM_GUARD_ALLOW_UNGUARDED_START`.

Non-JSON `/v1/*` request bodies (multipart, binary) are forwarded byte-for-byte
and are not inspected; only `application/json` bodies run through Nova Guard.
Request bodies over **8 MiB** are rejected with `413`.

## Platform-managed Nova Guard on the Worker

The Worker runs Nova Guard in **both** shapes:

* **Inline / stateless** — a `NOVEUM_GUARD_POLICIES` bundle of text policies
  (`regex_match`, `pii_detection`, secrets, banned terms, model allowlist, JSON
  schema, token limits), decided entirely from the payload in front of the
  gateway. Full parity with native.
* **Platform-managed** — set `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` and
  the Worker fetches the project's *effective* policy set, reads the live
  cost/rate counters, and reserves every request against the platform's
  **atomic admission API**. A `cost_cap` therefore holds across all 330+ PoPs,
  not per isolate. Implemented in `src/policy/worker_remote.rs` on
  `worker::Fetch` — no `reqwest`, no Durable Object, no Cloudflare binding.

### The request lifecycle

| Step | Call | Notes |
|---|---|---|
| 1. Policies | `GET /api/v1/projects/{id}/policies/effective` | Cached per isolate (~60 s) and revalidated with `If-None-Match`, so an unchanged set costs a `304`. A failed **refresh** keeps the last known set; a failed **first** fetch refuses the request (see the runbook). |
| 2. Live state | `GET .../policies/state` | Cached ~10 s. Unavailable state is *unavailable*, never "zero spend": `failClosed` policies block, fail-open policies allow with the reason recorded. |
| 3. Admission | `POST .../policies/admit` | Only when the set contains `cost_cap`/`rate_limit`. Reserves `estimatedInputTokens` + `maximumOutputTokens` atomically. **A block is HTTP 200** with `allowed:false`; a **503 is never an allow**. 2 s budget, one retry on a 5xx/429 (the same `requestId` replays, so it cannot double-reserve). |
| 4. Enforcement | in-process | The shared engine runs the input phase against the live counters, then the output phase — identical code to the native server. |
| 5. Settlement | `POST .../policies/reservations/{id}/{complete\|abandon\|cancel}` | Scheduled on `ctx.wait_until(...)` **before** the response is returned, so it never blocks the client and never floats. |

### Settlement semantics (identical to native)

| Outcome | Endpoint | Effect |
|---|---|---|
| Provider reported token counts (including an explicit `completion_tokens: 0`) | `complete` | Reconciles the hold **down** from `input + max_tokens` to what actually ran. |
| Stream ended with no usage frame, provider truncated, input-only report (Anthropic `message_start` then death), body past the 8 MiB inspection cap, non-JSON body | `abandon` | The conservative estimate **stays applied** — the call may have reached the provider. |
| A gateway-side policy blocked the request after admission | `cancel` | Releases the hold. This is the only place `cancel` is used: the call provably never reached the provider. |
| Client disconnected mid-stream | `abandon` | The teed body drops its sender, the settlement future resolves, the estimate stands. |

**Streaming usage is recovered without buffering.** The provider's SSE body is
teed: each chunk is fed to the shared `StreamUsageScanner`
(`src/policy/metering.rs`) and yielded onward in the same step, so bytes reach
the client unchanged, in order, with backpressure intact. The scanner reads
OpenAI's terminal `usage` chunk and Anthropic's `message_start`/`message_delta`
pair. While the bridge is metering, `stream_options.include_usage` is **forced**
on OpenAI streaming requests — without it a stream carries no counts at all and
every streamed request would settle at ~100x its real cost.

### Configuration outcomes

| Configuration | Worker behavior |
|---|---|
| `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` set | **Platform-managed Nova Guard.** Policies, live state and admission come from the control plane; any inline `NOVEUM_GUARD_POLICIES` is ignored (with a warning) — the platform is the source of truth. |
| Only *one* of the pair set, or either set to `""` | **503 `gateway_configuration_error`.** A half-applied bridge is still an attempt to enable enforcement; falling through to an unguarded proxy would reward the mistake with a 200. Same matrix as the native `RemoteConfig::from_values`. |
| Bridge set, but the **first** policy fetch fails | **503.** No policy set is known, so every request would be forwarded unguarded. Override with `NOVEUM_GUARD_ALLOW_UNGUARDED_START=true` (emergency only). |
| Bridge set, admission returns **503**/times out | `failClosed` decides: a fail-closed strict cap **blocks**; otherwise the request proceeds and the outage is logged. Never an implicit allow. |
| `NOVEUM_GUARD_POLICIES` containing `cost_cap` or `rate_limit`, **no** bridge | **503.** An inline bundle has no live spend/rate backend, so those policies could only evaluate to "allow" and their `failClosed` flag would be neutralized. Configure the bridge instead. |
| `NOVEUM_GUARD_POLICIES` set but not valid JSON / not a valid bundle | **503**, plus a structured `console_error`. Parsing the error away would silently drop every policy the operator deployed. |
| `NOVEUM_GUARD_POLICIES` unset (or empty), no bridge | Transparent proxy — the checked-in default. |
| `NOVEUM_GUARD_POLICIES` with text policies only | Enforced, identical to native. |

Request bodies are capped at **8 MiB**, the same bound as response inspection.
The declared `Content-Length` is checked before a byte is read, and the cap is
re-enforced while reading so a chunked body with no (or a lying) length cannot
allocate the isolate to death. Over the cap → **413**.

### Operational runbook

**Enable the bridge**

```bash
npx wrangler secret put NOVEUM_API_KEY     # paste a key with guardrails:read + guardrails:ingest
# NOVEUM_GUARD_PROJECT_ID is not a secret — put it in [vars] or:
npx wrangler secret put NOVEUM_GUARD_PROJECT_ID
npx wrangler deploy
```

`NOVEUM_API_KEY` must be a **secret**, never a `[vars]` entry: `[vars]` is
committed to `wrangler.toml` and readable in the dashboard. Both values are read
with `env.secret()` first and `env.var()` as a fallback, so either mechanism
works for the project id.

**Verify it is live**

```bash
curl -i $GW/v1/chat/completions -H "x-provider: openai" \
  -H "Authorization: Bearer $OPENAI_API_KEY" -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"max_tokens":16}'
```

Then check the project's usage in the Noveum dashboard: a completed request
appears with real token counts (not the `max_tokens` estimate). `npx wrangler
tail` streams the Worker's `console_*` lines, which name every admission block,
every fail-closed decision and every settlement that gave up.

**When something is wrong**

| Symptom | Cause | Action |
|---|---|---|
| Every `/v1/*` returns 503 `gateway_configuration_error` | Half-applied credentials, a blank secret, an inline `cost_cap`, or a malformed bundle — the message says which | Fix the named variable and redeploy. |
| 503 naming "policy set could not be fetched" | The control plane is unreachable or the key is rejected | Check the key's scopes and `NOVEUM_API_URL`. To keep serving *unguarded* meanwhile: `wrangler secret put NOVEUM_GUARD_ALLOW_UNGUARDED_START` → `true`. Caps are NOT enforced while it is set — unset it as soon as the fetch recovers. |
| Requests blocked with "platform admission unavailable … failing closed" | Admission returned 503 / exceeded its 2 s budget while a fail-closed strict cap is active | This is the policy working as written. Investigate the platform; setting the cap to fail-open trades enforcement for availability. |
| Spend looks ~100x too high on streaming | `stream_options.include_usage` was stripped by something between the Worker and the provider, so streams settle at their estimate | Check `wrangler tail` for `abandon` settlements on streaming requests. |
| `reservation … gave up after 3 attempts` in the logs | Settlement could not reach the platform | The hold expires server-side at `expiresAt` — conservatively, i.e. still counted until then. No action beyond fixing connectivity. |

Deleting the bridge is symmetrical: `npx wrangler secret delete NOVEUM_API_KEY`
**and** the project id. Removing only one leaves a half-applied configuration,
which is a deliberate 503.

## What runs on the edge today vs. next

**Working now — verified live on the global edge:**
- `/health`.
- **All OpenAI-compatible providers**: `openai, groq, together, fireworks,
  mistral, cohere, google/gemini, deepseek, xai/grok, openrouter, perplexity`
  (proxy + response pass-through).
- **Anthropic**: path → `/v1/messages`, `Authorization: Bearer` → `x-api-key` +
  `anthropic-version`, and the response converted back to OpenAI Chat Completions
  shape — same as the native server.
- **Bedrock**: OpenAI → Bedrock **Converse** request, **AWS SigV4**-signed in pure
  Rust (`sha2`+`hmac`; see `src/sigv4.rs`) with credentials from `x-aws-*` headers,
  and the Converse response converted back to OpenAI shape. Unlike native, the
  edge also accepts **temporary credentials** via `x-aws-session-token`. Pass
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
- **SSE streaming**: passed through unbuffered (input redaction still applies;
  output-phase enforcement is skipped on streams — the documented v1 limitation,
  identical to native). When the platform bridge is metering, the same bytes are
  teed through the shared usage scanner so the reservation settles at the
  stream's real token counts — still zero buffering.
- **Header + query pass-through**: client request headers (e.g. `OpenAI-Beta`,
  `OpenAI-Organization`, `anthropic-beta`) and the query string are forwarded;
  upstream response headers (`x-request-id`, rate-limit, …) are preserved. The
  transparent path forwards the request body byte-for-byte (only redacted bodies
  are re-serialized).
- **CORS**: permissive (`*`) on all responses + `OPTIONS` preflight, matching the
  native `CorsLayer`.
- **Nova Guard — input phase**: block + redact/mask (regex, PII, secrets, banned
  substrings, model allowlist, JSON-schema, token caps — the full shared engine).
  Input redactions are applied to the body *before* the upstream call.
- **Nova Guard — output phase**: block + redact on non-streaming responses
  (buffered up to an 8 MB inspection cap; larger responses pass through
  uninspected, matching native).
- **Platform-managed Nova Guard**: effective-policy fetch, live cost/rate state,
  atomic admission and reservation settlement — see the section above. *Not yet
  exercised against a live edge deployment*: its decision logic (admit
  classification, settlement choice, body caps, credential matrix) is unit-tested
  natively in `src/policy/worker_remote.rs`, and the `worker::Fetch` plumbing is
  proven only by `worker-build --release` and `wrangler deploy --dry-run`.

The same Nova Guard policy schema, decisions, and redactions run here as on the
native server (the engine + request/response shaping are one shared codebase).
Note the redact replacement key is **`redactWith`** (see `wrangler.toml`).

**All 13 providers now run on the edge** (OpenAI-compatible set + Anthropic +
Bedrock). **Next (see [CLOUDFLARE_DEPLOYMENT.md](CLOUDFLARE_DEPLOYMENT.md)):**
- Edge telemetry sink (Workers Analytics Engine / Queues) and Workers-KV policies.
- Hoist the pure halves of `policy::remote` / `policy::admission` into a shared
  wasm-safe module so `worker_remote.rs` stops carrying its own copy of the
  credential matrix, `classify_admit` and the settlement bodies.
- Re-enable `wasm-opt` to shrink the bundle (currently ~3.3 MB unoptimized;
  gzips to ~1 MB, well under the 10 MB paid-plan limit).
- Optionally add `x-aws-session-token` support to the **native** Bedrock path too
  (the edge already supports temporary credentials).
