# Noveum AI Gateway — Observability, Tracing & Capability Plan

**Scope.** When a caller supplies a Noveum API key + project, the gateway should
emit **Noveum-native traces + full LLM/operational telemetry** for every request —
for callers **with and without** the `noveum-trace` SDK — and expose the
operational signals a top-tier AI gateway is expected to have. This doc is the
research synthesis + a phased plan, benchmarked against the 2026 competitive
landscape. Nova Guard appears where it intersects observability; its own roadmap
is tracked separately but summarized in §8.

> Status of this revision: rewritten from the shallow first draft after a deep
> multi-front investigation — 17-product competitive matrix, the OTel GenAI
> conventions (spans **and** metrics), per-vendor telemetry catalogs (LiteLLM /
> Portkey / Cloudflare / Kong / Envoy / Helicone), voice/realtime tracing
> (Pipecat / LiveKit / OpenAI Realtime), the `noveum-trace` SDK wire format, the
> Noveum ingest contract, and an audit of our own gateway. Full findings in the
> appendices.

---

## Table of contents
1. Executive summary & strategy
2. Competitive landscape (matrix + what each does for observability)
3. What a production AI gateway should have (capability taxonomy)
4. Observability & telemetry spec (the core)
5. What the gateway CAN vs CANNOT capture (the boundary)
6. Current state of our gateway + NovaGuard (audit) + gap analysis
7. Implementation plan (mechanics) + phased roadmap
8. NovaGuard intersection
9. Open decisions
10. Appendices (raw research)

---

## 1. Executive summary & strategy

**The opportunity.** Every serious AI gateway (LiteLLM, Portkey, Cloudflare, Kong,
Vercel, TrueFoundry, Bifrost, LangDB, Plano/Arch, Envoy AI) now ships
observability as a first-class pillar — token/cost/latency telemetry, OpenTelemetry
GenAI export, and per-request logging. Several emit OTel **GenAI** spans/metrics
natively. We have a unique asset they don't: **a native trace backend
(`noveum-trace` + the Noveum platform)**. The gateway should become the
**zero-instrumentation on-ramp** to Noveum tracing — any caller routing through it
gets native Noveum traces, and (with the SDK) the gateway's inference span stitches
into the app's trace.

**Three strategic bets:**
1. **Noveum-native traces from the gateway** (mirror the SDK's `llm.*` wire shape →
   `POST /api/v1/traces`). Differentiator: traces look native in the Noveum UI with
   zero app code.
2. **Standards-compliant telemetry** (OTel GenAI spans + metrics, RED/USE
   Prometheus, OTLP export) so we're a peer of LiteLLM/Kong/Envoy and can also feed
   Datadog/Grafana/Langfuse. Dual-emit `llm.*` (Noveum) + `gen_ai.*` (OTel).
3. **Closed loop with Nova Guard** — the same gateway that traces also enforces
   policies and (via the platform bridge already built) governs cost/rate.

**Where we stand vs the field (high level):** we are strong on **multi-provider +
format translation** (13 providers incl. Anthropic + Bedrock SigV4), **edge/WASM**
(unique — only Cloudflare is edge-native), and **guardrails breadth** (Nova Guard:
regex/PII/secrets/banned/model-allowlist/json-schema/token-cap + cost/rate via the
platform). We are **behind** on: observability/tracing (no trace or metrics export
yet — only a console metrics plugin), routing/resiliency (no fallbacks/retries/LB/
load-balancing or circuit-breaking), and caching (none). This plan closes the
observability gap first (the request) and lays out the rest as a roadmap.

---

## 2. Competitive landscape (2026)

Legend ✓ full · ◐ partial · ✗ none. Sources + per-product detail in **Appendix D**.

| Product | Providers/translate | Routing/LB/fallback/CB | Cache (exact/sem) | Governance (RL/budget/keys/RBAC) | Guardrails (PII/inj/mod/DLP) | Prompt mgmt / A-B / evals | MCP | **Observability/Tracing** | Deploy | License |
|---|---|---|---|---|---|---|---|---|---|---|
| **LiteLLM** | ✓ 100+ | ✓ adaptive/LB/fallback, CB◐ | ✓/✓ | ✓ keys+budgets | ✓ Presidio/Lakera/secrets/schema | ◐/✓/integr | ✓ | ✓ **OTel v2 (GenAI) + 25+ callbacks + richest Prometheus** | self/managed | MIT |
| **Portkey** | ✓ 1600+ | ✓ conditional/LB/fallback/**CB** | ✓/✓ | ✓ virtual keys/budgets/RBAC | ✓ 60+ guardrails | ✓/✓/✓ | ✓ | ✓ **strong native dashboard** + Prometheus + OTel(traces, exp.) | managed+OSS gw | MIT gw |
| **Helicone** ⚠️maint. | ✓ 100+ | ✓ P2C/EWMA/failover | ✓/✗ | ◐ RL, RBAC ent | ◐ inj detect-only | ✓/✓/✓ | ◐ | ✓ **obs-first** (sessions, users, dashboards); **no outbound OTLP/Prom** | cloud+self | Apache gw |
| **Cloudflare AI GW** | ✓ ~23, `/compat` | ✓ dynamic routing/fallback/retry, CB✗ | ✓/✗(planned) | ◐ RL+**spend limits**+BYOK | ✓ Presidio/inj/Llama Guard/DLP | ✗/✓split/◐ | ✗ | ✓ analytics+logs+**OTLP (GenAI)**; **no latency percentiles** | **edge only** | proprietary |
| **Kong AI GW** | ✓ 16+, norm | ✓ 7 LB algos+**CB**(3.13) | ✓/✓ | ✓ token+**cost** RL/RBAC/SSO | ✓ sanitizer/prompt-guard/Lakera | ◐ | ✓ | ✓ **OTel GenAI spans**+Prometheus+Konnect | all modes | OSS+ent |
| **Vercel AI GW** | ✓ 200+/45 | ✓ uptime/latency/cost sort+fallback | ◐ | ✓ per-key budgets/BYOK/RBAC | ◐ (via AI SDK) | ✗ | ◐ | ✓ dashboard (TTFT/P75)+OTel via AI SDK + Drains | managed/edge | proprietary |
| **OpenRouter** | ✓ 400+ | ✓ cost-weighted/Auto/fallback | ✓/✗ | ✓ spend caps/keys/BYOK | ✓ PII redact/inj/schema | ◐ presets | ✓ | ◐ activity dash + opt-in logging + **Broadcast → 18 backends** | managed | proprietary |
| **TrueFoundry** | ✓ 250+ | ✓ weight/latency/priority+**CB** | ◐ | ✓ 3-layer RL/quotas/RBAC/SSO | ✓ **strongest** PII/PHI/inj/mod/secrets/SQL/OPA | ◐/✓/◐ | ✓ | ✓ **Prometheus+OTLP+Grafana+Tracing** (TTFT, inter-token) | SaaS/VPC/on-prem | commercial |
| **Bifrost (Maxim)** | ✓ 1000+ | ✓ failover/weighted LB/HA | ✓/✓ | ✓ virtual keys/budgets/SSO/RBAC | ◐ via integr | ◐ | ✓ | ✓ Prometheus(**cache_hits, TTFT**)+OTLP+dashboard; **~11–20µs overhead (Go)** | OSS self-host | Apache |
| **LangDB** | ✓ 250+ | ✓ fallback/latency/%/script, CB✗ | ◐ | ◐ RL/keys | ◐ early | ◐ | ◐ | ✓ **OTel-native → ClickHouse** + agent-trace UI | cloud+OSS (Rust) | flux |
| **Plano/Arch** (DO) | ✓ 15+ | ◐ intent/preference routing+session | ✗ | ✗ | ◐ Arch-Guard jailbreak/topic | ◐ Signals | ✓ | ✓ **OTel+Prometheus+Grafana+Signals™** (behavioral) | self/edge (Envoy) | Apache (Rust) |
| **Apache APISIX AI** | ✓ 6+ | ✓ RR/weighted/hash+health, CB◐ | ◐/✗ | ◐ token RL/keys | ◐ regex/mod add-on | ◐ | ◐ | ✓ Prometheus + OTel (**traces only**) | self/edge | Apache |
| **Azure APIM GenAI** | ✓ OpenAI/Anthropic/Vertex | ✓ pools RR/weighted/priority+**CB** | ✓/✓ (Redis) | ✓ token-limit/quota/Entra RBAC | ◐ Content Safety/prompt-shield | ◐/◐ | ✓ | ✓ token metrics→App Insights + prompt logging | managed PaaS | Azure |
| **AWS Bedrock** | ◐ Converse (Bedrock only) | ◐ cross-region; rest self-built | ◐ prompt-cache | ◐ IAM/Budgets | ✓ **Bedrock Guardrails** + Automated Reasoning | ✓/✓ native | ✓ Agents | ✓ CloudWatch GenAI metrics + invocation logging + X-Ray | managed | AWS |
| **Envoy AI GW** | ✓ via upstreams | ✓ Envoy LB/retry/**CB**/outlier/health | ✗ | ✓ token RL (CEL) | (via ext) | ✗ | ◐ | ✓ **OTel GenAI metrics** + full Envoy cluster stats | self/k8s | Apache |
| **Noveum (us)** | ✓ 13 + translate + **edge/WASM** | ✗ | ✗ | ◐ Nova Guard cost/rate (platform-bridged) | ✓ Nova Guard suite | ✗ | ✗ | ✗ **(this plan)** — console metrics only today | self/Docker/edge/lib | MIT/Apache |

**Market moves to know:** Helicone→Mintlify (Mar 2026, **maintenance mode**); Arch
→ **Plano** under DigitalOcean (Apr 2026); Vercel AI Gateway **GA** (Aug 2025);
Cloudflare added dynamic routing/guardrails/DLP/realtime/spend-limits; LiteLLM
shipped **OTel v2 (GenAI semconv)**; Martian/Unify/GPTRouter effectively dead as
routers. **Semantic caching** is the clearest dividing line (only LiteLLM/Portkey/
Kong/Azure/Bifrost have it). **Native inline PII/DLP** on Cloudflare/Kong/
TrueFoundry/OpenRouter/LiteLLM/Bedrock/Azure.

**Read for us:** observability + routing/resiliency + caching are table stakes we
lack; our edge/WASM + Nova Guard breadth + a native trace backend are our edges.

---

## 3. What a production AI gateway should have (capability taxonomy)

Distilled from the matrix. ✅ = we have it; 🟡 partial; ❌ = gap (priority noted).

**A. Connectivity** — multi-provider ✅, OpenAI-compatible surface ✅, format
translation (Anthropic/Bedrock↔OpenAI) ✅, streaming ✅, multimodal 🟡, batch ❌,
embeddings/rerank/audio endpoints 🟡.

**B. Routing & resiliency** — model routing ❌, load balancing (weighted/latency/
least-used) ❌ (P2), **fallbacks** ❌ (P1-resiliency), **retries + timeouts** 🟡
(reqwest has timeouts; no retry/fallback policy) (P1), circuit breaking / outlier
ejection ❌ (P2), provider health checks ❌ (P2), cross-region ❌.

**C. Caching** — exact-match ❌ (P2), semantic ❌ (P3). (Differentiator gap.)

**D. Governance** — rate limiting ❌-native (Nova Guard rate_limit via platform
state 🟡), budgets/cost caps 🟡 (Nova Guard cost_cap via platform), quotas ❌,
virtual/managed keys ❌ (P2 — big for multi-tenant), RBAC/SSO/teams ❌ (platform
concern), BYOK ✅ (keys are per-request).

**E. Security / guardrails** — PII redaction ✅, secret scanning ✅, prompt-injection
/ jailbreak detection 🟡 (regex/banned today; ML-based ❌ → P2), content moderation
❌ (P2), DLP 🟡, JSON-schema validation ✅, model allowlist ✅, token caps ✅.
(Strong relative to the field.)

**F. Prompt & experimentation** — prompt management/versioning ❌, A/B + canary ❌,
evals ❌ (platform has NovaEval — integrate, not rebuild).

**G. Agentic / tools** — MCP gateway ❌ (P3, rising fast — LiteLLM/Portkey/Kong/
Bifrost/TrueFoundry/Azure all have it), tool routing ❌, A2A ❌.

**H. Multimodal / realtime / voice** — image/audio passthrough 🟡, **realtime/voice
tracing** ❌ (P3; only Cloudflare addresses realtime at the gateway). Pipecat/
LiveKit are app-side; we'd consume their spans, not replace them (see §5).

**I. Observability** — **the focus of this plan**: tracing ❌, metrics export ❌
(console only), logging 🟡, OTLP/Prometheus ❌. (§4.)

**J. Deployment** — self-host/Docker ✅, Rust library ✅, **edge/WASM** ✅ (rare),
SaaS ❌ (platform), VPC/on-prem ✅.

---

## 4. Observability & telemetry spec (the core)

A best-in-class gateway emits **three signals**: **traces** (per-request spans),
**metrics** (RED/USE + GenAI histograms), and **logs** (structured request records).
We emit all three, Noveum-native first, OTel/Prometheus second.

### 4.1 Traces — Noveum-native (primary)

Mirror the `noveum-trace` SDK wire shape → `POST {NOVEUM_API_URL}/api/v1/traces`
(batch `{traces:[…]}`, ≤1000) or `/api/v1/traces/single`. Bearer key (org from
key), `traces:write` perm. **Async** (200=queued). ClickHouse `noveum_traces`/
`noveum_spans`. Full schema in **Appendix B**.

Per request → one trace, one root span `llm.<model>`. **IDs = UUID4 strings,
timestamps = ISO-8601** (NOT OTLP). `name` required; `project` required = project
**UUID**; `sdk = {name:"noveum-ai-gateway", version:<crate>}`.

**Span attributes (mirror the SDK's `llm.*`):** `llm.model`, `llm.provider`,
`llm.operation` (`chat`), `llm.input_tokens`/`output_tokens`/`total_tokens`,
`llm.cost.input`/`output`/`total`/`currency`, `llm.finish_reason`, `llm.streaming`,
`llm.time_to_first_token_ms`, `llm.latency_ms`, `llm.system_fingerprint`,
`llm.created`; content (opt-in) `llm.input.messages`/`llm.output.response`/
`llm.output.tool_calls`. Full SDK key list in **Appendix A**.

### 4.2 Traces — OTel GenAI (secondary, dual-emit)

Also emit the same span under OTel GenAI conventions for callers exporting to
Datadog/Grafana/Langfuse (every major gateway does this). Span name
`{operation} {model}` (e.g. `chat gpt-4o`); attrs `gen_ai.provider.name`,
`gen_ai.operation.name`, `gen_ai.request.model`/`.temperature`/`.max_tokens`/…,
`gen_ai.response.model`/`.id`/`.finish_reasons`, `gen_ai.usage.input_tokens`/
`output_tokens`, `gen_ai.response.time_to_first_chunk`, content via
`gen_ai.input.messages`/`output.messages` (opt-in). **Accept + emit both old and
new names** (`gen_ai.system`→`provider.name`, `prompt/completion`→`input/output`
tokens) — most instrumentation in the wild lags. Full reference in **Appendix E**.

### 4.3 Metrics (RED/USE + GenAI) — Prometheus `/metrics` + OTLP

This is what LiteLLM/Kong/Envoy/TrueFoundry expose and we have **zero** of today.
Target catalog (names follow OTel/Prometheus conventions; full per-vendor catalog
in **Appendix F**):

- **RED (request path):** `http.server.request.duration` histogram (buckets per
  OTel) → p50/p95/p99; request counter by `status_code`; failed-request counter by
  `error.type` (low-cardinality: `rate_limited`/`timeout`/`content_filter`/`auth`/
  `context_length`/`upstream_5xx`/`client_4xx`); `active_requests` gauge.
- **GenAI:** `gen_ai.client.token.usage` histogram (`gen_ai.token.type` in/out),
  `gen_ai.client.operation.duration`, `gen_ai.server.time_to_first_token`,
  `gen_ai.server.time_per_output_token` (all OTel-defined buckets); cost counter
  `gen_ai.cost.total` (USD) by model/provider.
- **Governance/Nova Guard:** remaining-budget + remaining-rate gauges (from the
  platform `/state`), guard block/redact counters by policy, rate-limit-429 counter.
- **Resiliency (as we add it):** retries counter, fallbacks counter tagging
  `fallback_model`, cooldown/circuit-open gauge, provider-health/deployment-state
  gauge (LiteLLM's `0=healthy/1=partial/2=outage` pattern).
- **Cache (as we add it):** hit/miss + semantic-hit counters, cache-latency histogram.
- **USE (runtime):** standard `process_*` + Tokio task/queue gauges.

Export: Prometheus `/metrics` (gated by `ENABLE_PROMETHEUS`) + OTLP (4317 gRPC /
4318 HTTP) so it lands in Grafana/Datadog/Honeycomb. Ship a Grafana dashboard.

### 4.4 Logs

Structured per-request log (provider, model, status, latency, tokens, cost,
request_id, cache status, retries/fallbacks taken). Content (prompt/response) **off
by default**, opt-in `NOVEUM_TRACE_CAPTURE_CONTENT` (privacy — matches OTel + every
vendor). Sink: stdout JSON + the Noveum trace (the trace IS the durable log).

### 4.5 Voice / realtime tracing

For OpenAI Realtime over **WebSocket** through the gateway, we can emit a voice
turn span with audio token accounting (`input_token_details.audio_tokens`,
output @50ms/100ms rates, cached). For **WebRTC**, media is SRTP-encrypted and
terminated at the provider — the gateway can't see it. The voice-specific timing
(time-to-first-**audio**, barge-in, VAD/EOU, turn structure) lives in the app
(Pipecat observers, LiveKit `set_tracer_provider`). **Design:** accept app-side
voice spans (Pipecat/LiveKit emit OTel) and **stitch the gateway's LLM span into
them on `conversation.id` + turn/`speech_id`**, rather than reconstruct voice
timing at the proxy. Recommended span hierarchy + the capture boundary in
**Appendix G**.

### 4.6 The trace wire format + propagation (the crux)

Three hard constraints (verified) determine the design:
1. **The `noveum-trace` SDK emits no trace context over the wire** (contextvars
   only; no `traceparent`, no header on outbound LLM calls, no provider-client
   auto-instrumentation). A downstream gateway sees **no Noveum id** today.
2. **The ingest is trace-immutable** — re-POSTing a `trace_id` is rejected
   (ReplacingMergeTree by `trace_id`). The gateway **can't append a span** to a
   finished SDK trace by re-POST.
3. **Noveum ids are UUIDs** (incompatible with W3C `traceparent`'s 32-hex). →
   a **Noveum-native propagation header** is required for Noveum merging.

**Header contract (proposed):** `x-noveum-trace-id` (UUID) + `x-noveum-parent-span-id`
(UUID) + optional `x-noveum-project-id`/`-session-id`/`-user-id`. Precedence:
present → **child-span mode**; absent → **new-trace mode**. Also accept W3C
`traceparent` for OTel-native interop (Noveum header wins).

**Three fidelities for "SDK present":**
- **(A) Linked separate trace — works today, no platform/SDK change.** Gateway
  mints its own trace + a span `link` → `{app_trace_id, app_span_id}`. Navigable,
  not unified. Caller passes the header manually (OpenAI SDK `default_headers`).
- **(B) Unified trace via span-append — needs a platform endpoint** (`POST
  /api/v1/traces/{id}/spans`) so the gateway contributes its inference span to the
  app's trace.
- **(C) Auto-propagation — needs an SDK enhancement** to inject the header on
  outbound LLM calls (it doesn't today).

---

## 5. What the gateway CAN vs CANNOT capture (the boundary)

| Signal | Gateway (proxy) | Needs in-app SDK |
|---|---|---|
| model, provider, request/response, finish_reason | ✅ | — |
| token usage (incl. streaming: OpenAI `include_usage`, Anthropic deltas, Groq `x_groq`) | ✅ | — |
| cost, latency, **TTFT**, status/errors | ✅ | — |
| **tool calls the model *requested*** (`tool_calls` in the response) | ✅ | — |
| WS-realtime audio token usage | ✅ | — |
| tool **execution** + result on the next turn | ❌ | ✅ |
| multi-step agent loops, RAG/retrieval, DB calls, app spans | ❌ | ✅ |
| WebRTC realtime media/usage | ❌ (SRTP terminated at provider) | ✅ |
| perceptual time-to-first-audio, barge-in, VAD/EOU, turn structure | ⚠️ partial | ✅ |

**Division of labor:** the gateway emits the **inference span (+ WS-realtime usage)**;
the SDK/framework emits agent/tool/retrieval/voice-timing spans. Linked via the
propagation header (§4.6) they form one trace. This matches the OTel GenAI
hierarchy (proxy emits `chat`; `invoke_agent`/`execute_tool` are app-side).

---

## 6. Current state of our gateway + NovaGuard (audit) + gap analysis

**What we have (v1.2.0):**
- **Providers (13):** OpenAI + OpenAI-compatible (Groq, Together, Fireworks, Mistral,
  Cohere, Gemini, DeepSeek, xAI, OpenRouter, Perplexity) + **Anthropic** (Messages
  transform) + **Bedrock** (Converse + AWS SigV4, incl. temporary creds on the edge).
- **Three deployment shapes** from one crate: native binary/Docker, Rust library,
  **Cloudflare Worker (WASM)** — edge is rare in the field.
- **Nova Guard** (shared engine, native + edge): regex_match, banned_substrings,
  model_allowlist, pii_detection, secrets_detection, json_schema, token_length_cap
  (all stateless, enforced) + cost_cap, rate_limit (stateful — **fail open** without
  a state backend; now satisfied by the **platform bridge** we built:
  `policy/platform.rs` + `policy/remote.rs` fetch policies + live `/state`).
  Block modes (synthetic_success/provider_error), input+output phases, redaction.
- **Pricing** table (`policy/pricing.rs`) → per-request cost (dual rate).
- **Telemetry:** `MetricsRegistry` + `MetricsExporter` trait + `metrics_middleware`
  building `RequestMetrics` (model, provider, tokens, cost, latency/TTFB,
  status, request/response bodies, streamed chunks, tracking ids). **Only a console
  exporter today.** A `to_otel_log()` flat record exists (non-OTLP).
- **Edge parity, CORS, header/query pass-through, streaming, 8 MB inspection cap.**

**NovaGuard platform Phase 0** (branch `feat/nova-guard-phase0-backend`): COST_CAP +
RATE_LIMIT CRUD (`/api/v1/projects/:id/policies`) + `/policies/state` (Redis
counters) + UI. **Gap on the platform side:** nothing *writes* the `guardrails:*`
counters yet (a usage→counter writer is a later phase) — so cost/rate enforcement
needs the counters populated.

**Gap analysis (vs §3 taxonomy / the field):**

| Area | Status | Priority |
|---|---|---|
| **Tracing export (Noveum + OTel)** | ❌ | **P1** |
| **Metrics export (Prometheus + OTLP)** | ❌ (console only) | **P1** |
| Structured request logging | 🟡 | P1 |
| Fallbacks / retries / timeouts policy | 🟡 (timeout only) | P1–P2 |
| Load balancing / routing / circuit breaking / health | ❌ | P2 |
| Exact + semantic caching | ❌ | P2 / P3 |
| Virtual/managed keys, quotas, RBAC | ❌ | P2 (multi-tenant) |
| ML prompt-injection / moderation | 🟡 (regex) | P2 |
| MCP / tool routing | ❌ | P3 |
| Prompt mgmt / A-B / evals | ❌ | platform (NovaEval) |
| Voice/realtime tracing | ❌ | P3 |
| NovaGuard cost/rate live enforcement | 🟡 (bridge built; needs platform counter-writer) | P2 |

---

## 7. Implementation plan (mechanics) + phased roadmap

**Extension point:** the `MetricsExporter` trait (`src/telemetry/metrics.rs`) — fan-out
is `tokio::spawn` per exporter, **fire-and-forget off the hot path**. `RequestMetrics`
already carries everything for the LLM span. Add exporters, don't rewire.

**Cross-cutting mechanics:**
- **Dedicated reqwest client** for telemetry egress (NOT `proxy::CLIENT`, which forces
  `http2_prior_knowledge` and breaks against HTTP/1.1 backends — the bug we already
  hit + fixed in the Nova Guard bridge).
- **Batched background exporter** (BatchSpanProcessor pattern: bounded queue, flush on
  delay/size/shutdown ForceFlush; queue-full = drop, never block). Never await
  telemetry on the request path.
- **Config:** `NOVEUM_API_KEY`, `NOVEUM_API_URL`, `NOVEUM_TRACE_PROJECT_ID` (UUID),
  `NOVEUM_TRACING_ENABLED`, `NOVEUM_TRACE_CAPTURE_CONTENT`, `NOVEUM_TRACE_SAMPLE_RATE`,
  `NOVEUM_SERVICE_VERSION`, `ENABLE_PROMETHEUS`, `OTEL_EXPORTER_OTLP_ENDPOINT`.
- **Read inbound propagation headers** in `metrics_middleware` (none read today).
- **Edge (WASM):** emit via `worker::Fetch` using the same **pure** trace/metric
  builders; only the HTTP send differs per target.

**Phases:**
- **P1 — Tracing + metrics MVP (this request).**
  1. `NoveumTraceExporter` → standalone Noveum trace per request (`llm.*` span),
     batched async, content opt-in, sampling. *(SDK-absent case — the 80% win.)*
  2. Prometheus `/metrics` exporter (RED + GenAI token/cost/latency/TTFT) + OTLP.
  3. Structured request log.
- **P2 — Linked traces + OTel spans + resiliency + governance telemetry.**
  Read the propagation header → emit gateway trace with a `link` to the app trace
  (fidelity A). Dual-emit OTel `gen_ai.*` spans. Add fallbacks/retries (with
  telemetry: retry/fallback/cooldown counters, provider-health gauge). Surface
  Nova Guard cost/rate + budget gauges from the platform `/state`. Virtual keys.
- **P3 — Unified traces + caching + MCP + voice.** Platform span-append endpoint +
  SDK header auto-injection → unified SDK+gateway traces; `include_usage` injection
  for exact streaming tokens. Exact then semantic caching (with cache telemetry).
  MCP/tool routing. Voice: accept Pipecat/LiveKit OTel spans + stitch on
  `conversation.id`.
- **P4 — Advanced routing/LB/circuit-breaking, evals (NovaEval) wiring, multimodal.**

---

## 8. NovaGuard intersection

Nova Guard and tracing are the **closed loop**: traces show what happened; Nova
Guard governs it. Concretely: (a) emit a **guard span/event** on the trace when a
policy blocks/redacts (policy id, action, reason) — so the Noveum UI shows
enforcement inline; (b) the cost/rate telemetry (§4.3) reads the same platform
`/state` the Nova Guard bridge already uses; (c) the platform's missing
**usage→guardrails-counter writer** can be fed by the gateway's own usage
emission (the gateway already computes tokens/cost per request) — i.e. tracing and
governance share the usage pipeline. Sequence Nova Guard live-enforcement (needs
the counter-writer) alongside P2.

---

## 9. Open decisions

1. **P1 scope:** ship standalone Noveum tracing **and** Prometheus/OTLP metrics
   together, or tracing first?
2. **Dual-emit OTel `gen_ai.*`** spans from the start (interop) or Noveum `llm.*`
   only first? (Recommend `llm.*` first, `gen_ai.*` in P2.)
3. **Propagation header** `x-noveum-trace-id`/`-parent-span-id` (+ accept
   `traceparent`) — confirm.
4. **Content capture** off by default (privacy) — confirm.
5. **Unified SDK+gateway traces (P3):** OK to add a platform span-append endpoint +
   SDK header injection?
6. **Project association:** require `NOVEUM_TRACE_PROJECT_ID` (UUID) per deployment
   and/or accept `x-noveum-project-id` per request (multi-tenant)?
7. **Resiliency vs caching ordering** in P2/P3 — which first?

---
---

# APPENDICES — RAW RESEARCH

## Appendix A — `noveum-trace` Python SDK spec (v1.5.17)

Attribute keys are inline string literals (no central constants file). **No HTTP
trace-context propagation** — only contextvars in-process; the only outbound
headers are on its own export POST (`Authorization: Bearer`, `User-Agent:
noveum-trace-sdk/<ver>`, `Content-Type`). **Span "type" = span-name prefix**
(`llm.*`/`tool.*`/`agent.*`/`chain.*`/`retrieval.*`/`routing.*`); **no `span.kind`**.
**IDs = UUID4 strings.** Namespace **`llm.*`** (not `gen_ai.*`).

**LLM span attrs:** `llm.model`, `llm.provider`, `llm.operation`; `llm.input.<k>`/
`llm.output.<k>`; `llm.input_tokens`/`output_tokens`/`total_tokens`;
`llm.cost.input`/`output`/`total`/`currency`; `llm.context_window`,
`llm.max_output_tokens`, `llm.finish_reason`, `llm.system_fingerprint`,
`llm.created`. LangChain integration adds the canonical rich set:
`llm.input.temperature`/`prompts`/`messages`/`message_count`/`has_tool_calls`,
`llm.available_tools.count`/`names`/`schemas`, `llm.output.response`/`finish_reason`/
`tool_calls`/`tool_calls.count`/`names`, `llm.streaming`, `llm.latency_ms`,
`llm.time_to_first_token_ms`. **Tool calls** default to inline records appended to
the parent LLM span (keys: name/operation/input/output/status/tool_call_id/error/
called_tool/available_tools/function_definition), not separate spans. **Agent**:
`agent.id`/`type`/`name`/`operation`/`capabilities` (+ agent_graph.*/agent_workflow.*).
**Retrieval**: `retrieval.query`/`result_count`/… **Streaming**:
`streaming.tokens_received`/`time_to_first_token`/`tokens_per_second`/…
**Exceptions**: `exception.type`/`message`/`stacktrace` + event.

**Naming/status:** `llm.<model>`, `tool.<name>`, `agent.<name>`, `retrieval.<name>`,
`chain.<name>`, `routing.<src>_to_<tgt>`; status enum
`unset|ok|error|timeout|cancelled` → `status` + `status_message`.

**Instrumentation:** context managers (`trace_llm_call`, `trace_agent_operation`,
`trace_operation`, `trace_context`, `create_child_span`), manual
(`start_trace`/`start_span`), agents, streaming, threads. **No OpenAI/Anthropic
monkey-patching;** callback auto-instrumentation only for **LangChain**, plus
CrewAI/LiveKit/Pipecat listeners.

**Assembly/flush:** whole trace exported as one object on finish (spans nested in
`Trace.spans`); single→`/v1/trace`, batch→`/v1/traces` `{"traces":[…],"timestamp":…}`;
retries on 429/5xx. `sdk = {"name":"noveum-trace-python","version":"1.5.17"}`;
top-level `service_version`/`project`/`environment` if configured; endpoint
`NOVEUM_ENDPOINT` (default `https://api.noveum.ai/api`) + `/v1/traces`.

## Appendix B — Platform ingest API + gateway telemetry

**Ingest:** `POST /api/v1/traces` (batch `{traces:[…]}`, ≤1000) + `/api/v1/traces/single`
(one Trace). Bearer key → `db.apiKey.findFirst`; org from key; needs `traces:write`;
mismatched `X-Organization-*` → 403. **Async** via BullMQ → ClickHouse
`noveum_traces` + `noveum_spans` (ReplacingMergeTree; org isolation via
`organization_slug`/`organization_id`). **200 = queued, not persisted.**

**Trace schema:** `trace_id`(UUID, server-gen if absent), `name`**(required; empty→
dropped)**, `start_time`/`end_time`(ISO), `duration_ms`, `status`(`ok|error|timeout`),
`status_message?`, `span_count`, `error_count?`, `project`**(required = project
UUID; unknown id auto-creates junk project)**, `environment?`(def production),
`service_version?`(top-level; stored, queryable `?service_version=` — the
agent-version mechanism), `sdk`**(required `{name,version}`)**, `attributes?`,
`metadata?{user_id,session_id,request_id,tags,custom_attributes}`, `spans`(required).
**Span:** `span_id`(UUID),`trace_id`,`parent_span_id?`,`name`,`start/end`(ISO),
`duration_ms`,`status`,`status_message?`,`attributes?`,`events?[{name,timestamp,
attributes}]`,`links?[{trace_id,span_id,attributes}]`. **Gotchas:** re-POST of an
existing `trace_id` silently rejected (immutable); rate-limit 1000 pts/60s/IP (429);
per-org span quota.

**Gateway telemetry:** `metrics_middleware` → `RequestMetrics` → `record_metrics`
fans out to each `MetricsExporter` via `tokio::spawn` (fire-and-forget).
`MetricsExporter` trait `async export_metrics(RequestMetrics)` + `name()` is the
extension point. `ProviderMetrics`: `input/output/total_tokens`, `cost`, `model`,
`provider_latency`, `request_id`, `project_id`/`organization_id`/`user_id`/
`experiment_id` (**no finish_reason field** — in raw JSON). Per-provider extractors
parse OpenAI `usage.*`, Anthropic `input/output_tokens` (+ streaming thread_local),
Bedrock `inputTokens/outputTokens` + `x-amzn-RequestId`, Groq `x_groq.usage` +
`usage.total_time` latency, Fireworks. Cost via `policy::pricing::estimate_cost`.
`metrics_middleware` captures provider (`x-provider`), tracking ids (`x-project-id`/
`x-organization-id`/`x-user-id`/`x-experiment-id`), bodies+sizes, TTFB,
`provider_request_id` (`x-request-id`||`request-id`); 30s timeout (504 not
recorded); streaming usage from chunks (Anthropic/Groq full; OpenAI only with
include_usage else `len/4` estimate); **no metrics → no record → no trace.** **No
inbound trace context read today.** `RequestMetrics` carries full request/response
bodies + `streamed_data`; a `to_otel_log()` (flat, non-OTLP) already exists.

## Appendix C — LLM-observability platforms + propagation (prior round)

**OTel gen_ai (span attrs)** — see Appendix E (refreshed). **Platforms:** Langfuse
(traces/sessions/users/scores, prompt mgmt, evals), LangSmith, Arize Phoenix
(OTel-native), Traceloop/OpenLLMetry (emits gen_ai spans; lags on names), Datadog
LLM Obs, Braintrust, Helicone (sessions via `Helicone-Session-Id/-Path/-Name`),
Portkey (native dashboard). **Propagation patterns:** W3C `traceparent`
(keep trace-id, new span-id, parent=incoming) — LiteLLM honors it; Helicone
`Helicone-Session-Id`; Portkey `x-portkey-trace-id`/`-span-id`/`-parent-span-id`
(+ W3C); LiteLLM `metadata.trace_id`/`existing_trace_id`. **Streaming usage:**
`stream_options:{include_usage:true}` → final usage chunk (empty `choices[]`);
proxies inject it upstream + strip if unrequested; tokenizer fallback = estimate;
Anthropic spreads usage across `message_start`/`message_delta`. **Non-blocking:**
BatchSpanProcessor (5s/2048/512 defaults; ForceFlush on shutdown).

## Appendix D — Competitive matrix detail + per-product observability profiles

(17 products; see §2 matrix.) Per-product observability emphasis:

- **LiteLLM** — OTel-first + 25+ callbacks; spans `litellm_request`/`raw_gen_ai_request`/
  per-guardrail; TTFT/TPOT; `gen_ai.cost.*` breakdown; OTel v2 GenAI semconv;
  richest Prometheus (see Appendix F). Privacy `turn_off_message_logging`.
- **Portkey** — strongest native dashboard; 40+ metrics incl. cache status
  (miss/hit/**semantic hit**), retry/fallback/LB state; OTel **inbound** ingest.
- **Helicone** ⚠️maint. — obs-first; sessions/users first-class; deepest UI but
  **no outbound OTLP/Prometheus for LLM telemetry**; frozen.
- **Cloudflare** — analytics (requests/tokens/cost/errors/cache%) + logs + **OTLP
  GenAI**; trace-link headers `cf-aig-otel-trace-id`/`-parent-span-id`; **no latency
  percentiles** (gap); cost = token estimate.
- **Kong** — OTel GenAI spans (`kong.gen_ai`) + Prometheus (`ai_llm_*`) + Konnect;
  audit-log `time_to_first_token`/`time_per_token`; cache-status header.
- **Vercel** — dashboard (TTFT, P75 duration/TTFT) + OTel via AI SDK + Drains.
- **OpenRouter** — light native; **Broadcast** → 18 backends (Langfuse/Datadog/
  OTLP/ClickHouse/S3…); default ZDR.
- **TrueFoundry** — Prometheus (`ai_gateway_requests_total`, `first_token_latency_ms`,
  `inter_token_latency_ms`, cost) + OTLP + Grafana (ID 23388) + tracing.
- **Bifrost** — native Prometheus (`bifrost_cost_total`, `cache_hits_total`,
  `stream_first_token_latency_seconds`) + OTLP + dashboard; ~11–20µs overhead (Go).
- **LangDB** — OTel-native → **ClickHouse**; agent-trace UI.
- **Plano/Arch** — OTel + Prometheus + Grafana + **Signals™** (model-free behavioral
  indicators: misalignment/loops/tool-failures/timeouts) + W3C trace context.
- **Envoy AI** — OTel GenAI metrics + full Envoy cluster/circuit-breaker/outlier/
  health stats.
- **Azure APIM** — token metrics → App Insights (+ streaming); KQL.
- **AWS Bedrock** — CloudWatch GenAI metrics + invocation logging + X-Ray.

## Appendix E — OpenTelemetry GenAI conventions reference (2026)

Moved to `open-telemetry/semantic-conventions-genai`; **Development** status;
`OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`.

**Span names:** `{operation} {model}` (chat/embeddings); `execute_tool {tool.name}`;
`invoke_agent {agent.name}`; `create_agent {agent.name}`.
**`gen_ai.operation.name`:** chat, generate_content, text_completion, embeddings,
execute_tool, create_agent, invoke_agent, invoke_workflow, plan, retrieval,
create_memory/search_memory/… **`gen_ai.provider.name`:** anthropic, aws.bedrock,
azure.ai.openai, cohere, deepseek, gcp.gemini, gcp.vertex_ai, groq, mistral_ai,
openai, perplexity, x_ai, …
**Request:** `gen_ai.request.model`, `.temperature`, `.top_p`, `.top_k`,
`.max_tokens`, `.frequency_penalty`, `.presence_penalty`, `.stop_sequences`,
`.seed`, `.stream`, `.choice.count`, `.reasoning.level`.
**Response:** `gen_ai.response.id`, `.model`, `.finish_reasons`(array),
`.time_to_first_chunk`(s, streaming TTFT).
**Usage:** `gen_ai.usage.input_tokens`, `.output_tokens`,
`.cache_read.input_tokens`, `.cache_creation.input_tokens`, `.reasoning.output_tokens`.
**Conversation:** `gen_ai.conversation.id` (don't fabricate). **Tools:**
`gen_ai.tool.name`(req), `.call.id`, `.type`(function/extension/datastore),
`.description`. **Agents:** `gen_ai.agent.id`/`name`/`description`/`version`.
**Content (opt-in):** `gen_ai.input.messages`, `gen_ai.output.messages`,
`gen_ai.system_instructions`.
**Metrics:** `gen_ai.client.token.usage` (hist; `gen_ai.token.type` in/out),
`gen_ai.client.operation.duration`, `gen_ai.server.time_to_first_token`,
`gen_ai.server.time_per_output_token` (each with defined buckets), `gen_ai.server.
request.duration`. **Deprecated→new (accept both):** `gen_ai.system`→`provider.name`;
`usage.prompt_tokens`→`input_tokens`; `usage.completion_tokens`→`output_tokens`;
`gen_ai.prompt`/`completion`→removed (use input/output.messages);
`request.is_stream`→`request.stream`; per-message events→aggregated messages.

## Appendix F — Operational telemetry catalog (RED/USE + per-vendor)

**Methodology:** RED (Rate/Errors/Duration) for the request path; USE
(Utilization/Saturation/Errors) for runtime; OTel HTTP `http.server.request.duration`
histogram (buckets `[.005,.01,.025,.05,.075,.1,.25,.5,.75,1,2.5,5,7.5,10]`s);
`error.type` low-cardinality (status class / `timeout` / domain id / `_OTHER`).
**OTel GenAI metrics buckets:** token usage `[1,4,16,64,256,1024,…,67108864]`;
operation duration `[.01,.02,.04,.08,.16,.32,.64,1.28,…,81.92]`s; TTFT
`[.001,.005,.01,.02,.04,.06,.08,.1,.25,.5,.75,1,2.5,5,7.5,10]`s; TPOT
`[.01,.025,.05,.075,.1,.15,.2,.3,.4,.5,.75,1,2.5]`s.
**Prometheus:** Counter/Gauge/Histogram/Summary; histograms aggregate across
instances (→ fleet p99 via `histogram_quantile`), summaries don't; base units, `_total`
suffix; `/metrics`. **OTLP:** gRPC 4317 / HTTP 4318; Traces+Metrics+Logs stable.

**LiteLLM (reference Prometheus set):** `litellm_spend_metric`,
`litellm_total_tokens_metric`, `litellm_input/output_tokens_metric`,
`litellm_input_cached_tokens_metric`, `litellm_output_reasoning_tokens_metric`;
`litellm_requests_metric`, `litellm_request_total_latency_metric`,
`litellm_llm_api_latency_metric`, `litellm_llm_api_time_to_first_token_metric`,
`litellm_overhead_latency_metric`; `litellm_proxy_total_requests_metric`
(label status_code), `litellm_proxy_failed_requests_metric` (exception_status/class);
`litellm_deployment_state` (**gauge 0/1/2**), `litellm_deployment_cooled_down`,
`litellm_deployment_successful_fallbacks`/`_failed_fallbacks` (labels requested_model/
fallback_model/exception_*); `litellm_remaining_requests/tokens_metric`,
budget gauges; `litellm_in_flight_requests`, `litellm_redis_latency`,
`litellm_self_latency`. **Kong:** `ai_llm_requests_total`, `ai_llm_cost_total`,
`ai_llm_tokens_total{token_type}`, `ai_llm_provider_latency`, `ai_cache_*_latency` +
`kong_*_latency_ms`, `kong_upstream_target_health` (gauge). **Envoy:** OTel GenAI
metrics + cluster `upstream_rq_total/active/timeout/retry*`, `circuit_breakers.*`
(`cx_open`/`rq_open`), `outlier_detection.ejections_*`, `health_check.*`. **Portkey:**
`request_count{cacheStatus,…}`, stage-latency histograms, `llm_cost_sum`,
`llm_token_sum`; status codes 429 (rate) vs 412 (budget). **Cloudflare:** logs API
fields `duration`/`status_code`/`cost`/`tokens_in/out`/`cached`/`step`; **no latency
percentiles**. **Helicone:** Compare-Models API p99/p95/p90/median/avg + ttft; **no
outbound OTLP/Prom**. **Headers (cross-vendor, for did-fallback/cache):** Portkey
`x-portkey-retry-attempt-count`/`-last-used-option-index`/`-cache-status`; Cloudflare
`cf-aig-step`/`-cache-status`; Helicone `Helicone-Fallback-Index`/`-Cache`; Kong
`X-Kong-*-Latency`/`X-Cache-Status`.

## Appendix G — Voice / realtime tracing

**Recommended span hierarchy:** `conversation/session (conversation.id, channel)` →
`turn (number, duration, was_interrupted, speech_id, e2e_latency)` → {`stt`
(transcript, audio_duration, is_final), `eou` (end_of_utterance_delay,
transcription_delay), `llm` (gen_ai.* + ttft + token split incl. audio/cached),
`execute_tool`, `tts` (voice_id, time-to-first-audio, character_count, audio_duration)}.

**Pipecat:** OTel tracing — one trace = conversation; spans `conversation→turn→
{stt,llm,tts}` (or `llm_setup`/`llm_response`/`llm_tool_call` for speech-to-speech);
attrs use **deprecated `gen_ai.system`** + custom `metrics.ttfb`. Metrics:
TTFB/Processing/LLMUsage/TTSUsage/Turn. Observers: `TurnTrackingObserver`,
`UserBotLatencyObserver`. **LiveKit Agents:** OTel via `set_tracer_provider`;
dual-namespace `lk.*` (rich: `lk.response.ttft`, `lk.response.ttfb`, `lk.e2e_latency`,
EOU/interruption/AMD timing) + 5 official `gen_ai.*`; metrics LLM/TTS/STT/EOU/VAD/
Realtime; e2e ≈ `eou_delay + llm.ttft + tts.ttfb`; targets P90<3.5s/TTFT<800ms.
**OpenAI Realtime:** `response.done.usage` (text/audio/cached token details; input
audio @100ms, output @50ms; transcription billed separately in a different event).
**WS** transport → a proxy sees all events + usage; **WebRTC** → SRTP terminated at
the provider, **not interceptable**.

**Boundary:** gateway captures the LLM/text/usage layer (esp. WS realtime); voice
timing (time-to-first-audio, barge-in, VAD/EOU, turn structure) + WebRTC need an
in-app SDK. **Backend should accept both and stitch on `conversation.id` +
`speech_id`/turn id**, normalizing deprecated `gen_ai.system`/token names on ingest.
