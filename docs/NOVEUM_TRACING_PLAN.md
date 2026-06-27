# Noveum Tracing in the AI Gateway — Design & Plan

Goal: when a caller supplies a Noveum API key + project, the gateway emits a
**Noveum-native trace per LLM request** (same wire shape + `llm.*` attributes the
`noveum-trace` SDK produces) to the Noveum platform, so gateway-origin telemetry
looks native in the Noveum UI — for callers **with and without** the SDK.

This document is the research synthesis + the implementation plan. Nova Guard is
out of scope here (separately tracked).

---

## 1. The two cases we must serve

| Case | What the gateway should do |
|---|---|
| **No SDK** (caller only uses the gateway) | Gateway is the *only* observer → it mints a **standalone trace** (1 trace = 1 request) with a root `llm.*` span. |
| **SDK present** (caller runs `noveum-trace` *and* routes through the gateway) | The app already has a trace + spans. The gateway's inference span should attach to **that** trace (no duplicate / orphan). Requires trace-context propagation. |

---

## 2. What a gateway CAN and CANNOT capture (verified against LiteLLM/Helicone/Portkey/Cloudflare/OTel)

A proxy parses the OpenAI-format request/response itself — no app SDK needed for
the **inference span**. But it only sees HTTP, not in-process state.

**CAN capture** (everything for the LLM-call span):
- `model`, `provider`, request `messages`/prompt, response content
- token usage — incl. streaming (OpenAI `stream_options.include_usage`, Anthropic
  `message_start`/`message_delta` deltas, Groq `x_groq.usage`)
- cost (gateway pricing table), latency + **TTFT**, `finish_reason`, status/errors
- **tool calls the model *requested*** (`finish_reason: "tool_calls"` + the
  `tool_calls` array in the response) — i.e. *that* a tool was asked for.

**CANNOT capture** (in-app only — needs the SDK):
- tool **execution** + its result on the next turn ("sees the request to call a
  tool, not what the tool did")
- multi-step agent loops / orchestration, RAG/retrieval, DB calls, app spans.

→ Division of labor: **gateway emits the inference span; the SDK emits
agent/tool/retrieval/chain spans.** Together (when linked) they form the full
trace. This matches the OTel GenAI hierarchy (a proxy emits the `chat` span;
`invoke_agent`/`execute_tool` are emitted in-app and never traverse the proxy).

---

## 3. The Noveum trace wire format (the contract the gateway must produce)

Ingest: `POST {NOVEUM_API_URL}/api/v1/traces` (batch `{ "traces": [...] }`, ≤1000)
or `/api/v1/traces/single` (one trace). `Authorization: Bearer <key>`; org is
resolved **from the key**; requires the `traces:write` permission. **Async** —
`200` = *queued*, not persisted. Stored in ClickHouse `noveum_traces` /
`noveum_spans`.

**Trace object** (required unless noted):
- `trace_id` (UUID; server generates if omitted)
- `name` *(required — empty name → the whole trace is silently dropped)*
- `start_time` / `end_time` — **ISO-8601 strings**
- `duration_ms` (≥0), `status` (`"ok" | "error" | "timeout"`), `status_message?`
- `span_count`, `error_count?`
- `project` *(required — must be the **project UUID**; an arbitrary string
  auto-creates a junk project)*
- `environment?` (default `"production"`), `service_version?` (top-level sibling —
  this is the agent/version tracking field; set a git SHA / semver)
- `sdk` *(required)* — `{ name, version }`
- `attributes?`, `metadata?` `{ user_id?, session_id?, request_id?, tags?, custom_attributes? }`
- `spans` *(required)* — array of Span

**Span object:** `span_id` (UUID), `trace_id`, `parent_span_id?`, `name`,
`start_time`/`end_time` (ISO), `duration_ms`, `status`, `status_message?`,
`attributes?`, `events?` `[{name,timestamp,attributes}]`, `links?`
`[{trace_id,span_id,attributes}]`.

**IDs are UUID4 strings; timestamps are ISO-8601 strings** — NOT OTLP 32-hex /
epoch-ns. This is the single most important compatibility fact (see §5).

### 3.1 Span attribute conventions (mirror the SDK exactly)

The SDK uses the **`llm.*`** namespace (NOT `gen_ai.*`), no `span.kind` — the
operation type is the **span-name prefix**. For an LLM-call span:

- name: **`llm.<model>`** (e.g. `llm.gpt-4o-mini`)
- `llm.model`, `llm.provider`, `llm.operation` (`"chat"`)
- `llm.input_tokens`, `llm.output_tokens`, `llm.total_tokens`
- `llm.cost.input`, `llm.cost.output`, `llm.cost.total`, `llm.cost.currency` (`"USD"`)
- `llm.finish_reason`, `llm.streaming` (bool), `llm.time_to_first_token_ms`, `llm.latency_ms`
- `llm.system_fingerprint`, `llm.created`, `llm.context_window`, `llm.max_output_tokens`
- content (opt-in, off by default): `llm.input.messages`, `llm.output.response`,
  `llm.output.tool_calls`, `llm.output.tool_calls.names`, `llm.input.temperature`

(Span prefixes for the SDK's other types, which the gateway does **not** emit:
`tool.<name>`, `agent.<name>`, `retrieval.<name>`, `chain.<name>`.)

`sdk` = `{ name: "noveum-ai-gateway", version: "<crate version>" }` — a **distinct
name** so gateway-origin traces are identifiable vs. SDK (`noveum-trace-python`).

---

## 4. The propagation problem (the crux) — findings + constraints

Two hard facts from the research collide:

1. **The `noveum-trace` SDK emits NO trace context over the wire.** It keeps the
   trace/span ids in a `contextvars.ContextVar` and never injects `traceparent`,
   `tracestate`, or any `x-*-trace` header onto the app's outbound LLM call. It
   has **no auto-instrumentation** of the OpenAI/Anthropic clients. So a
   downstream gateway finds **no Noveum id on the request** today.

2. **The platform ingest is trace-immutable.** Re-POSTing an existing `trace_id`
   is silently rejected, and `noveum_traces` is a `ReplacingMergeTree` keyed on
   `trace_id`. So the gateway **cannot append a span** to a trace the SDK already
   created/finished by re-POSTing that `trace_id`.

3. **Noveum ids are UUIDs**, incompatible with W3C `traceparent` (32-hex
   trace-id / 16-hex span-id). So `traceparent` cannot carry a Noveum trace-id —
   a Noveum-native propagation header is required for Noveum merging (W3C is for
   OTel-native backends).

**Consequence:** true "merge the gateway's span into the SDK's trace" needs **two
new capabilities** that don't exist yet — a propagation header *and* a
span-append ingest path. We therefore phase it:

### Propagation header contract (proposed)
```
x-noveum-trace-id:        <uuid>   # the caller's current trace id
x-noveum-parent-span-id:  <uuid>   # the caller's current span id (gateway span parents to it)
x-noveum-project-id:      <uuid>   # project (else NOVEUM_GUARD/TRACE_PROJECT_ID)
x-noveum-session-id / x-noveum-user-id  # optional metadata passthrough
```
Gateway precedence: if `x-noveum-trace-id` present → **child-span mode**; else →
**new-trace mode**. (We also *accept* W3C `traceparent` for OTel-native interop,
mapping it to a separate OTel-style trace; Noveum-native header wins if both
present — Portkey's exact precedence rule.)

### Three ways to realize "SDK present", in increasing fidelity
- **(A) Linked separate trace — works TODAY, no platform/SDK change.** Gateway
  mints its own trace and adds a span `link` → `{app_trace_id, app_span_id}` (the
  schema supports links). Navigable cross-trace reference in the UI; not unified.
  Needs only the caller to pass `x-noveum-trace-id`/`-parent-span-id` (which they
  can do manually via the OpenAI SDK's `default_headers`/`extra_headers`).
- **(B) Unified trace via span-append — needs a platform endpoint.** Platform
  adds `POST /api/v1/traces/{trace_id}/spans` (append spans to an existing trace,
  bumping `span_count`). Gateway posts just its inference span with the caller's
  `trace_id` + `parent_span_id` → one unified trace.
- **(C) Auto-propagation — needs an SDK enhancement.** The SDK injects the
  propagation header on outbound LLM calls (requires it to wrap the provider
  client, which it doesn't today) so callers get (A)/(B) with zero app code.

---

## 5. Implementation in the gateway (mechanics)

The clean extension point already exists: the **`MetricsExporter` trait**
(`src/telemetry/metrics.rs`). `metrics_middleware` builds a `RequestMetrics` per
request and fans out to every exporter via `tokio::spawn` — **fire-and-forget,
off the hot path** (network I/O to the ingest API can't add latency to the
proxied call). `RequestMetrics` already carries everything we need: provider,
model, input/output/total tokens, cost, total/provider latency + TTFB,
status_code, request/response bodies, `streamed_data`, and the tracking ids
(`x-project-id`, `x-organization-id`, `x-user-id`, `provider_request_id`).

Plan:
1. **`NoveumTraceExporter`** (new `src/telemetry/plugins/noveum_trace.rs`) impl
   `MetricsExporter`. Builds a Noveum trace from `RequestMetrics`:
   - mint `trace_id`/`span_id` (UUID4) **unless** the request carried
     `x-noveum-trace-id`/`-parent-span-id` (child-span / linked mode)
   - one root span `llm.<model>` with the §3.1 `llm.*` attributes; `status` from
     `status_code` (+ sniff `response_body.error`); `duration_ms` from latency;
     `llm.time_to_first_token_ms` from TTFB; `finish_reason` mined from the
     response body
   - content attributes only when `NOVEUM_TRACE_CAPTURE_CONTENT=true` (privacy)
2. **Inbound headers** — extend `metrics_middleware` to read the propagation
   headers into `RequestMetrics` (it does not read any inbound trace context
   today; only the `x-project-id`/`x-user-id` tracking headers).
3. **Emission** — start fire-and-forget per request; add a **batched background
   exporter** (BatchSpanProcessor pattern: bounded queue, flush on
   delay/size/shutdown ForceFlush) → `POST /api/v1/traces` batch. Use a
   **dedicated reqwest client** (NOT `proxy::CLIENT`, which forces
   `http2_prior_knowledge` and breaks against HTTP/1.1 control planes — same bug
   we hit + fixed in the Nova Guard bridge).
4. **Config:** `NOVEUM_API_KEY`, `NOVEUM_API_URL` (default `https://api.noveum.ai`),
   `NOVEUM_TRACE_PROJECT_ID` (the project UUID), `NOVEUM_TRACING_ENABLED`,
   `NOVEUM_TRACE_CAPTURE_CONTENT`, `NOVEUM_TRACE_SAMPLE_RATE`, `NOVEUM_SERVICE_VERSION`.
5. **Streaming token accuracy (phase 3):** optionally inject
   `stream_options.include_usage=true` upstream when the client didn't, parse the
   final usage chunk, and **strip it** before forwarding so the client's stream
   shape is unchanged; tokenizer fallback marked as an estimate.
6. **Edge (wasm Worker):** emit via `worker::Fetch` using the same shared
   trace-builder (the builder is pure; only the HTTP send differs per target).

### Boundaries / honest limitations
- Requests that record **no usage** (cancelled stream with no final chunk),
  **504 timeouts**, and non-JSON responses currently produce no metrics → no
  trace. Decide whether to still emit an error-only span for these.
- `finish_reason`, tool_calls, and message content live only in the raw
  request/response JSON — mined there, not first-class fields.
- The gateway emits **one inference span**; agent/tool/RAG spans remain the SDK's
  responsibility.

---

## 6. Phasing

- **Phase 1 — Standalone gateway traces (no SDK).** `NoveumTraceExporter` →
  one trace per request with the `llm.*` inference span, async batched emission,
  content opt-in, config + sampling. **Fully shippable and high-value on its own.**
- **Phase 2 — Linked traces (SDK present, option A).** Read the propagation
  headers; emit the gateway trace with a `link` to the caller's trace.
  No platform/SDK change; callers pass the header manually.
- **Phase 3 — Unified traces (option B + C).** Platform adds a span-append
  endpoint; SDK injects the propagation header automatically; gateway contributes
  its span to the caller's trace. Plus `include_usage` injection for exact
  streaming tokens.

---

## 7. Open decisions (need product input)

1. **Propagation/merge approach:** ship Phase 1 now; for SDK-present, do we go
   (A) linked-trace (no deps, navigable) first, then (B) unified via a platform
   span-append endpoint? (B) and (C) require platform + SDK work.
2. **Header contract:** confirm `x-noveum-trace-id` / `x-noveum-parent-span-id`
   (Noveum-native, UUID) as primary, with W3C `traceparent` accepted for interop.
3. **Content capture default:** off (privacy) with opt-in — confirm.
4. **Attribute namespace:** mirror the SDK's `llm.*` (recommended, native in the
   Noveum UI) vs. also dual-writing OTel `gen_ai.*`. Recommend `llm.*` only for
   Noveum; add `gen_ai.*` only if we later export to OTel backends.
5. **Project association:** require `NOVEUM_TRACE_PROJECT_ID` (UUID) per gateway
   deployment, and/or accept `x-noveum-project-id` per request for multi-tenant.
