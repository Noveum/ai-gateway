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

Non-JSON `/v1/*` request bodies (multipart, binary) are forwarded byte-for-byte
and are not inspected; only `application/json` bodies run through Nova Guard.

### Nova Guard scope on the Worker: stateless policies only

The Worker supports **stateless, inline** Nova Guard — the text policies
(`regex_match`, `pii_detection`, secrets, banned terms, model allowlist, JSON
schema, token limits) that are decided entirely from the payload in front of the
gateway. Those run at full parity with the native server.

**Anything that needs cross-request state is explicitly out of scope on this
deployment target**, and the Worker refuses `/v1/*` with a **503
`gateway_configuration_error`** rather than accepting the configuration and
enforcing nothing:

| Configuration | Worker behavior |
|---|---|
| `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` set | **503.** The platform bridge (remote policy fetch, live cost/rate state, usage reporting, admission ledger) is not compiled for `wasm32`. |
| Only *one* of `NOVEUM_API_KEY` / `NOVEUM_GUARD_PROJECT_ID` set | **503.** A half-applied bridge configuration is still an attempt to enable enforcement; falling through to an unguarded proxy would reward the mistake with a 200. |
| `NOVEUM_GUARD_POLICIES` containing `cost_cap` or `rate_limit` | **503.** There is no live spend/rate backend, so these can only ever evaluate to "allow" — and their `failClosed` flag is neutralized along with them. |
| `NOVEUM_GUARD_POLICIES` set but not valid JSON / not a valid bundle | **503**, plus a structured `console_error`. Parsing the error away would silently drop every policy the operator deployed. |
| `NOVEUM_GUARD_POLICIES` unset (or empty) | Transparent proxy — the checked-in default. |
| `NOVEUM_GUARD_POLICIES` with text policies only | Enforced, identical to native. |

Every refusal above is deliberate: the failure mode they replace is an operator
believing a hard cap, a fail-closed policy, or *any* policy set is in force at
the edge while every request passes. **Use the native gateway for
platform-managed Nova Guard and for any cost/rate enforcement.**

Supporting them here would require a Worker-native state plane — a Durable Object
for atomic reservation/reconciliation, plus a `wasm32` HTTP path to the Noveum
API for policy fetch, live state and usage reporting. That is tracked as future
work below and is not part of the current release scope; a successful
`worker-build`/Wrangler dry-run only proves the refusal path compiles.

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
  identical to native).
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

The same Nova Guard policy schema, decisions, and redactions run here as on the
native server (the engine + request/response shaping are one shared codebase).
Note the redact replacement key is **`redactWith`** (see `wrangler.toml`).

**All 13 providers now run on the edge** (OpenAI-compatible set + Anthropic +
Bedrock). **Next (see [CLOUDFLARE_DEPLOYMENT.md](CLOUDFLARE_DEPLOYMENT.md)):**
- Edge telemetry sink (Workers Analytics Engine / Queues) and Workers-KV policies.
- Re-enable `wasm-opt` to shrink the bundle (currently ~3.3 MB unoptimized;
  gzips to ~1 MB, well under the 10 MB paid-plan limit).
- Optionally add `x-aws-session-token` support to the **native** Bedrock path too
  (the edge already supports temporary credentials).
