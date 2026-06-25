# Deploying the Noveum AI Gateway on Cloudflare (Distributed Edge)

Research + migration plan for running the gateway as a globally‑distributed
Cloudflare service. Current as of **June 2026**.

## TL;DR — chosen direction

**Primary: native Cloudflare Workers (Rust → WASM).** Containers add container
cold‑start + higher cost and are not true per‑PoP, so they are **not** the fast
path; we target the native Worker (V8 isolates, ~0 cold start, every PoP). We
**keep the existing native binary + Docker image fully working** for self‑hosting
(no Cloudflare Containers dependency), sharing **one Nova Guard engine** across
both builds so they never diverge.

| | **Native Worker (WASM)** — primary edge | **Native binary / Docker** — self‑host (kept) |
|---|---|---|
| Runtime | V8 isolate, `wasm32-unknown-unknown` via `workers-rs` | Tokio + Axum + reqwest (today's binary) |
| Distribution | **True per‑PoP edge**, 330+ cities, ~0 cold start | Wherever the operator runs it |
| Outbound HTTP | `worker::Fetch` | `reqwest` |
| Crypto (Bedrock SigV4) | **Web Crypto** (`crypto.subtle` HMAC‑SHA256) | `aws-sigv4` |
| Token caps | heuristic / wasm‑safe tokenizer | `tiktoken-rs` |
| Built from | new `worker/` crate | existing `noveum-ai-gateway` crate (published, unchanged) |

**Shared core:** the Nova Guard engine (regex/PII/secrets/banned/model‑allowlist/
json‑schema), pricing/cost, and provider base‑URL/path mapping live in one place
and compile to **both** native and wasm32, so the edge Worker and the self‑hosted
binary enforce identically.

> Cloudflare **Containers** remain a documented fallback (run the same Docker
> image) for anyone who wants the full native feature set without the WASM build —
> but they are not the primary deployment.

> Why not "just compile it to a Worker"? Cloudflare Workers run on V8 isolates and
> execute Rust as **WebAssembly (`wasm32-unknown-unknown`)**. That environment has
> **no threaded async runtime (no Tokio), no native sockets, no `reqwest`/`hyper`,
> and no native crypto (`ring`/`aws-lc`)**. Our current stack is built on exactly
> those. So a Worker port is a real engineering project, not a recompile.

---

## Implementation status

**Phase 1 — DONE (single package, three targets).** The `noveum-ai-gateway` crate
now compiles to `wasm32-unknown-unknown` as a Cloudflare Worker *and* to the
native server, from one codebase:
- Cargo deps split by target (`cfg(not(target_arch = "wasm32"))` = native server;
  `cfg(target_arch = "wasm32")` = `worker`); the native binary is gated by a
  `native` feature so the wasm build (`--no-default-features`) excludes it.
- Native-only modules (`proxy`, `handlers`, `config`, `providers`, `telemetry`,
  `error`, axum router) are `cfg`-gated; the **Nova Guard engine + pricing +
  `routing`** are shared and compile to both.
- `src/worker_rt.rs` is the `#[event(fetch)]` entry: `/health`, Nova Guard input
  enforcement via the shared engine, and proxy of the OpenAI-compatible providers
  via `worker::Fetch`.
- **Verified locally in `workerd` (`wrangler dev`):** real OpenAI + Groq proxy and
  the SSN block behave identically to the native server. Native: 166 unit + 10
  integration tests green; clippy clean on **both** targets; `worker-build`
  produces a deployable bundle.

See **[CLOUDFLARE_WORKER.md](CLOUDFLARE_WORKER.md)** for build/test/deploy steps.

**Remaining:** output-phase + SSE streaming on the edge; Anthropic + Bedrock
(Web-Crypto SigV4); edge telemetry sink + Workers-KV policies; `wasm-opt`.

## How Cloudflare runs code (the constraint that drives everything)

- **Workers = V8 isolates.** Rust is supported via [`workers-rs`](https://github.com/cloudflare/workers-rs)
  (`worker` crate, currently 0.8.x) compiled to `wasm32-unknown-unknown` with
  `wrangler`. Async/await works, but there is **no Tokio runtime / no threads /
  no `mio` / no `tokio::net` / no `tokio::time` multi‑thread**. `tokio::sync`
  primitives are fine. Outbound HTTP must use **`worker::Fetch`** (the platform
  fetch API), not `reqwest`/`hyper`. Crypto must use the **Web Crypto API**
  (`crypto.subtle`), not `ring`/`aws-lc-rs`.
- **Containers = real Linux containers** at the edge (GA since Apr 2026). Run **any
  x86‑64 Docker image unmodified**, fronted by a Worker, placed in the optimal
  location. Requires Workers Paid; billed per 10 ms of active CPU.

### Relevant platform limits (Workers)
- Memory: **128 MB hard**. CPU: **30 s default, 5 min hard** (per invocation).
- Subrequests: **10,000** default now (was 1,000); free plan = 50 external. A
  gateway makes ~1 upstream call/request, so this is a non‑issue.
- **WASM bundle size** matters — large embedded data (e.g. tiktoken BPE tables)
  risks the bundle/startup limits.

Sources: see bottom.

---

## Dependency reality check (this codebase → `wasm32-unknown-unknown`)

| Dependency / subsystem | On Workers (WASM)? | Action for a Worker port |
|---|---|---|
| `tokio` (features = full, rt-multi-thread) | ❌ pulls in `mio`/native net; won't compile | Drop the runtime; use the `worker` executor + `tokio` with only `sync`/`macros` if needed |
| `reqwest` + `hyper` (proxy client) | ❌ not on Workers | Rewrite outbound calls with **`worker::Fetch`** |
| `axum` (server) + `tower-http` | ⚠️ axum *routing* works via the `http` feature; server/compression do not | Use the `worker` router (or `axum` with `worker` http shim); drop `tower-http` compression |
| `aws-sigv4` + `aws-credential-types` (Bedrock signing) | ❌ needs native crypto | **Reimplement SigV4 with Web Crypto** (HMAC‑SHA256 via `crypto.subtle`) |
| `tiktoken-rs` (token_length_cap) | ⚠️ large embedded BPE + `fancy-regex`; bundle/wasm risk | Replace with a lightweight heuristic or a WASM‑safe tokenizer; or keep precise counts only on Containers |
| `jsonschema` (json_schema policy) | ⚠️ verify wasm32 build | Test; swap for a wasm‑friendly validator if needed |
| `regex`, `aho-corasick` | ✅ pure Rust | Keep (Nova Guard regex/banned/secrets/PII) |
| `arc-swap`, `parking_lot`, `bytes`, `serde`, `serde_json`, `thiserror`, `futures(-util)`, `async-stream` | ✅ | Keep |
| `chrono` (timestamps) | ⚠️ `now()` needs the wasm/js path | Use `worker::Date` or `js-sys::Date` for time |
| `uuid` v4 | ⚠️ needs `getrandom` `js` feature on wasm | Add `getrandom = { features = ["js"] }` for wasm, or use `worker`/Web Crypto randomness |
| `tracing-subscriber`, `colored`, `num_cpus`, `dotenv` | ❌ server/CLI only | Drop on Workers; log via `console_log!`/`worker` |
| `aws_event_stream_parser` (Bedrock streaming) | ⚠️ parsing is pure but tied to the Bedrock path | Port with the Bedrock rewrite (or keep Bedrock on Containers) |

**What ports cleanly:** the whole **Nova Guard deterministic engine** (regex,
banned substrings, secrets, PII, model allowlist — all `regex`/`aho-corasick`),
the **pricing/cost table**, and the request/response transform logic are pure
Rust and are the easy, high‑value part to run at the edge.

**What is hard:** the **proxy core** (`reqwest`→`worker::Fetch`, including
streaming SSE pass‑through), **Bedrock SigV4** (→ Web Crypto), **tiktoken**
(token caps), and **time/RNG** wasm features.

---

## Recommended plan (phased)

### Phase 0 — Decision + scaffolding
- [ ] Confirm the target: Containers‑first (fast) vs Workers‑native (edge‑max) vs both.
- [ ] Create a Cloudflare account on **Workers Paid** ($5/mo) — required for both
      Containers and meaningful Workers usage.
- [ ] Add `wrangler` + a `wrangler.toml` (or `.jsonc`) to the repo.

### Phase 1 — Cloudflare Containers (fast global win, ~no code changes)
- [ ] Reuse the existing `Dockerfile` (already builds a static `x86_64` binary on
      `rust:1.96`). Ensure it binds `0.0.0.0:$PORT` (it does, via `HOST`/`PORT`).
- [ ] Add a thin **Worker + Durable Object** front that routes incoming requests to
      a `Container` binding (the standard Cloudflare Containers pattern).
- [ ] Wire env/secrets (provider keys are passed per‑request via headers, so the
      container itself needs little config beyond `NOVEUM_GUARD_*`).
- [ ] Load‑test, confirm streaming pass‑through works through the Worker→Container hop.
- [ ] **Outcome:** the full gateway (all 13 providers incl. Bedrock, full Nova
      Guard) runs globally, unmodified. Trade‑off: container cold‑starts + higher
      cost than isolates; not literally per‑PoP.

### Phase 2 — Workers‑native core (the real edge play)
Build a parallel `worker`‑based crate (workspace member or feature‑gated build)
that handles the **OpenAI‑compatible providers** (OpenAI, Groq, Together,
Fireworks, Mistral, Cohere, Gemini, DeepSeek, xAI, OpenRouter, Perplexity — i.e.
everything except Bedrock):
- [ ] Scaffold with `worker` 0.8.x + `wrangler`; target `wasm32-unknown-unknown`.
- [ ] **Router:** port the `/v1/*` + `/health` routes to the `worker` router.
- [ ] **Proxy:** reimplement `proxy_request_to_provider` using `worker::Fetch`
      (`base_url + transform_path(path) + query`, header passthrough, **streaming
      response pass‑through** via `worker::Response` streaming).
- [ ] **Nova Guard:** compile the existing `policy` engine to wasm32 (regex/PII/
      secrets/banned/model‑allowlist all port directly). Replace `tiktoken-rs`
      (token caps) with a heuristic or wasm‑safe tokenizer; verify/replace
      `jsonschema`.
- [ ] **Metrics:** keep per‑request token/cost extraction (pure); ship telemetry
      via a Worker‑friendly path (e.g. a subrequest, Workers Analytics Engine, or
      Queues) instead of the Tokio‑spawned exporter.
- [ ] **Time/RNG:** `worker::Date`/`getrandom js` for timestamps + ids.
- [ ] Optimize WASM bundle size (`wasm-opt`, `default-features = false`, strip).
- [ ] **Outcome:** the most common providers run as true per‑PoP edge Workers
      (instant cold start, cheapest, lowest latency worldwide).

### Phase 3 — Bedrock on Workers (optional, hardest)
- [ ] Reimplement **AWS SigV4** signing with **Web Crypto** (`crypto.subtle`
      HMAC‑SHA256 chain) and port the Bedrock Converse request/response + event‑
      stream transforms to `worker::Fetch`.
- [ ] Until then, **route `x-provider: bedrock` to the Container** (a Worker can
      fall back to the Container binding for providers not yet ported) — a clean
      hybrid.

### Phase 4 — State & distribution niceties (optional)
- [ ] Hosted **Nova Guard policies** at the edge via **Workers KV** (replace the
      local file/inline bundle; cache‑on‑read, global propagation) — this also
      revives the deferred "hosted policies" idea without a custom control plane.
- [ ] **Durable Objects** for any cross‑request state we later want (e.g. real
      `rate_limit`/`cost_cap` counters — the live‑state seam already exists in the
      engine).
- [ ] Per‑PoP caching, WAF, and Cloudflare rate‑limiting in front.

---

## Architecture (target hybrid)

```
            ┌──────────── Cloudflare global edge (330+ PoPs) ───────────┐
client ───▶ │  Worker (WASM, workers-rs)                                │
            │   • routes /v1/* + /health                                │
            │   • Nova Guard (regex/PII/secrets/allowlist/json/token)   │
            │   • OpenAI-compatible providers via worker::Fetch ────────┼──▶ OpenAI/Groq/…
            │   • x-provider: bedrock ──▶ Container binding ────────────┼──▶ (Phase 1/3)
            │   • policies from Workers KV; metrics via Analytics/Queue │
            └───────────────────────────────────────────────────────────┘
```

## Concrete code-change checklist (Workers port)

- `src/main.rs` → replace `#[tokio::main]` + `TcpListener` with a `#[event(fetch)]`
  entry; drop the banner/`colored`/`num_cpus`.
- `src/proxy/client.rs`, `src/proxy/mod.rs` → delete `reqwest`/`hyper`; implement
  with `worker::Fetch` + `worker::Response` (incl. streaming).
- `src/proxy/signing.rs` (`aws-sigv4`) → Web Crypto SigV4 (Phase 3) or container‑only.
- `src/policy/rules/token_length_cap.rs` (`tiktoken-rs`) → heuristic/wasm tokenizer.
- `src/telemetry/*` → replace Tokio‑spawned exporter with a Worker‑native sink.
- `Cargo.toml` → wasm feature set: `default-features = false` everywhere; add
  `worker`, `getrandom = { features=["js"] }`; gate native‑only deps behind a
  `#[cfg(not(target_arch = "wasm32"))]` / Cargo feature so the **native binary
  (Containers / `cargo install`) keeps working unchanged**.

> Keep both targets in one crate via features (`native` vs `worker`) so the
> published crate + Docker/Container build stay intact while the Worker build is
> added.

## Open questions / decisions for the team
1. **Containers‑first, Workers‑first, or both?** (Recommendation: both, phased.)
2. **Bedrock at the edge** worth the Web‑Crypto SigV4 rewrite, or keep on Containers?
3. **Token caps** precision on Workers — heuristic acceptable, or required exact?
4. **Telemetry sink** on Workers — Analytics Engine, Queues, or subrequest to Noveum?
5. **Policy distribution** — move to Workers KV now (and retire the local‑file model at the edge)?

## Sources
- [workers-rs (GitHub)](https://github.com/cloudflare/workers-rs)
- [Cloudflare Workers — Rust language support](https://developers.cloudflare.com/workers/languages/rust/)
- [Supported crates · Workers Rust](https://developers.cloudflare.com/workers/languages/rust/crates/)
- [`worker` crate docs](https://docs.rs/worker/latest/worker/)
- [Workers platform limits](https://developers.cloudflare.com/workers/platform/limits/)
- [Workers are no longer limited to 1000 subrequests (Feb 2026)](https://developers.cloudflare.com/changelog/post/2026-02-11-subrequests-limit/)
- [Cloudflare Containers — product](https://www.cloudflare.com/products/containers/)
- [Cloudflare Containers — pricing](https://developers.cloudflare.com/containers/pricing/)
- [workers-rs issue #736 — wasm compile (mio/getrandom) pitfalls](https://github.com/cloudflare/workers-rs/issues/736)
- [Making Rust Workers reliable (Cloudflare blog)](https://blog.cloudflare.com/making-rust-workers-reliable/)
