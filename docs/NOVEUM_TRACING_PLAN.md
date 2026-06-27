# Noveum Tracing in the AI Gateway — Design, Plan & Research

Goal: when a caller supplies a Noveum API key + project, the gateway emits a
**Noveum-native trace per LLM request** (same wire shape + `llm.*` attributes the
`noveum-trace` SDK produces) to the Noveum platform, so gateway-origin telemetry
looks native in the Noveum UI — for callers **with and without** the SDK.

Part I is the synthesis + plan. Part II (appendices) is the full research:
the SDK spec, the platform ingest contract, the gateway's existing telemetry, and
the competitive landscape. Nova Guard is out of scope here (separately tracked).

---
---

# PART I — DESIGN & PLAN

## 1. The two cases we must serve

| Case | What the gateway should do |
|---|---|
| **No SDK** (caller only uses the gateway) | Gateway is the *only* observer → it mints a **standalone trace** (1 trace = 1 request) with a root `llm.*` span. |
| **SDK present** (caller runs `noveum-trace` *and* routes through the gateway) | The app already has a trace + spans. The gateway's inference span should attach to **that** trace (no duplicate / orphan). Requires trace-context propagation. |

## 2. What a gateway CAN and CANNOT capture (verified vs LiteLLM/Helicone/Portkey/Cloudflare/OTel)

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
trace. Matches the OTel GenAI hierarchy: a proxy emits the `chat` span;
`invoke_agent`/`execute_tool` are emitted in-app and never traverse the proxy.

## 3. The Noveum trace wire format (the contract the gateway must produce)

Ingest: `POST {NOVEUM_API_URL}/api/v1/traces` (batch `{ "traces": [...] }`, ≤1000)
or `/api/v1/traces/single` (one trace). `Authorization: Bearer <key>`; org is
resolved **from the key**; requires the `traces:write` permission. **Async** —
`200` = *queued*, not persisted. Stored in ClickHouse `noveum_traces` / `noveum_spans`.

**Trace object** (required unless noted):
- `trace_id` (UUID; server generates if omitted)
- `name` *(required — empty name → the whole trace is silently dropped)*
- `start_time` / `end_time` — **ISO-8601 strings**
- `duration_ms` (≥0), `status` (`"ok" | "error" | "timeout"`), `status_message?`
- `span_count`, `error_count?`
- `project` *(required — must be the **project UUID**; an arbitrary string
  auto-creates a junk project)*
- `environment?` (default `"production"`), `service_version?` (top-level sibling —
  the agent/version tracking field; set a git SHA / semver)
- `sdk` *(required)* — `{ name, version }`
- `attributes?`, `metadata?` `{ user_id?, session_id?, request_id?, tags?, custom_attributes? }`
- `spans` *(required)* — array of Span

**Span object:** `span_id` (UUID), `trace_id`, `parent_span_id?`, `name`,
`start_time`/`end_time` (ISO), `duration_ms`, `status`, `status_message?`,
`attributes?`, `events?` `[{name,timestamp,attributes}]`, `links?`
`[{trace_id,span_id,attributes}]`.

**IDs are UUID4 strings; timestamps are ISO-8601 strings** — NOT OTLP 32-hex /
epoch-ns. The single most important compatibility fact (see §4).

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

(SDK span prefixes the gateway does **not** emit: `tool.<name>`, `agent.<name>`,
`retrieval.<name>`, `chain.<name>`.)

`sdk` = `{ name: "noveum-ai-gateway", version: "<crate version>" }` — a **distinct
name** so gateway-origin traces are identifiable vs. the SDK (`noveum-trace-python`).

## 4. The propagation problem (the crux) — findings + constraints

Three hard facts collide:

1. **The `noveum-trace` SDK emits NO trace context over the wire.** It keeps the
   trace/span ids in a `contextvars.ContextVar`, never injects `traceparent`,
   `tracestate`, or any `x-*-trace` header onto the app's outbound LLM call, and
   has **no auto-instrumentation** of the OpenAI/Anthropic clients. A downstream
   gateway finds **no Noveum id on the request** today.
2. **The platform ingest is trace-immutable.** Re-POSTing an existing `trace_id`
   is silently rejected, and `noveum_traces` is a `ReplacingMergeTree` keyed on
   `trace_id`. So the gateway **cannot append a span** to a trace the SDK already
   created by re-POSTing that `trace_id`.
3. **Noveum ids are UUIDs**, incompatible with W3C `traceparent` (32-hex
   trace-id / 16-hex span-id). A Noveum-native propagation header is required for
   Noveum merging (W3C is for OTel-native backends).

**Consequence:** a *true* "merge the gateway's span into the SDK's trace" needs
**two new capabilities** that don't exist yet — a propagation header *and* a
span-append ingest path. We phase it.

### Propagation header contract (proposed)
```
x-noveum-trace-id:        <uuid>   # the caller's current trace id
x-noveum-parent-span-id:  <uuid>   # the caller's current span id (gateway span parents to it)
x-noveum-project-id:      <uuid>   # project (else NOVEUM_TRACE_PROJECT_ID)
x-noveum-session-id / x-noveum-user-id  # optional metadata passthrough
```
Precedence: if `x-noveum-trace-id` present → **child-span mode**; else →
**new-trace mode**. Also accept W3C `traceparent` for OTel interop (Noveum header
wins if both present — Portkey's rule).

### Three ways to realize "SDK present", increasing fidelity
- **(A) Linked separate trace — works TODAY, no platform/SDK change.** Gateway
  mints its own trace and adds a span `link` → `{app_trace_id, app_span_id}`
  (schema supports links). Navigable cross-trace reference; not unified. Caller
  passes the header manually (OpenAI SDK `default_headers`/`extra_headers`).
- **(B) Unified trace via span-append — needs a platform endpoint.** Platform
  adds `POST /api/v1/traces/{trace_id}/spans`. Gateway posts just its inference
  span with the caller's `trace_id` + `parent_span_id` → one unified trace.
- **(C) Auto-propagation — needs an SDK enhancement.** The SDK injects the header
  on outbound LLM calls (requires it to wrap the provider client, which it
  doesn't today) so callers get (A)/(B) with zero app code.

## 5. Implementation in the gateway (mechanics)

The clean extension point exists: the **`MetricsExporter` trait**
(`src/telemetry/metrics.rs`). `metrics_middleware` builds a `RequestMetrics` per
request and fans out via `tokio::spawn` — **fire-and-forget, off the hot path**.
`RequestMetrics` already carries provider, model, in/out/total tokens, cost,
total/provider latency + TTFB, status_code, request/response bodies,
`streamed_data`, and tracking ids (`x-project-id`, `x-organization-id`,
`x-user-id`, `provider_request_id`).

Plan:
1. **`NoveumTraceExporter`** (`src/telemetry/plugins/noveum_trace.rs`) impl
   `MetricsExporter`. Builds a Noveum trace from `RequestMetrics`:
   - mint `trace_id`/`span_id` (UUID4) unless the request carried the propagation
     headers (child-span / linked mode);
   - one root span `llm.<model>` with §3.1 attributes; `status` from `status_code`
     (+ sniff `response_body.error`); `duration_ms` from latency;
     `llm.time_to_first_token_ms` from TTFB; `finish_reason` mined from the body;
   - content attributes only when `NOVEUM_TRACE_CAPTURE_CONTENT=true` (privacy).
2. **Inbound headers** — extend `metrics_middleware` to read the propagation
   headers into `RequestMetrics` (no inbound trace context is read today).
3. **Emission** — fire-and-forget per request initially; then a **batched
   background exporter** (BatchSpanProcessor pattern: bounded queue, flush on
   delay/size/shutdown ForceFlush) → `POST /api/v1/traces` batch. Use a
   **dedicated reqwest client** (NOT `proxy::CLIENT`, which forces
   `http2_prior_knowledge` and breaks against HTTP/1.1 control planes — the same
   bug we hit + fixed in the Nova Guard bridge).
4. **Config:** `NOVEUM_API_KEY`, `NOVEUM_API_URL` (default `https://api.noveum.ai`),
   `NOVEUM_TRACE_PROJECT_ID` (project UUID), `NOVEUM_TRACING_ENABLED`,
   `NOVEUM_TRACE_CAPTURE_CONTENT`, `NOVEUM_TRACE_SAMPLE_RATE`, `NOVEUM_SERVICE_VERSION`.
5. **Streaming token accuracy (phase 3):** optionally inject
   `stream_options.include_usage=true` upstream when the client didn't, parse the
   final usage chunk, **strip it** before forwarding; tokenizer fallback as estimate.
6. **Edge (wasm Worker):** emit via `worker::Fetch` using the same shared
   trace-builder (pure builder; only the HTTP send differs per target).

### Boundaries / honest limitations
- Requests with no recoverable usage (cancelled stream), 504 timeouts, and
  non-JSON responses currently record no metrics → no trace. Decide whether to
  still emit an error-only span.
- `finish_reason`, tool_calls, message content live only in raw request/response
  JSON — mined there.
- The gateway emits **one inference span**; agent/tool/RAG spans stay the SDK's job.

## 6. Phasing

- **Phase 1 — Standalone gateway traces (no SDK).** `NoveumTraceExporter` → one
  trace/request with the `llm.*` inference span, async batched emission, content
  opt-in, config + sampling. **Fully shippable and high-value on its own.**
- **Phase 2 — Linked traces (SDK present, option A).** Read propagation headers;
  emit the gateway trace with a `link` to the caller's trace. No platform/SDK change.
- **Phase 3 — Unified traces (B + C).** Platform span-append endpoint + SDK header
  auto-injection; plus `include_usage` injection for exact streaming tokens.

## 7. Open decisions (need product input)

1. **Propagation/merge approach:** ship Phase 1 now; for SDK-present go (A)
   linked-trace first, then (B) unified via a platform span-append endpoint?
2. **Header contract:** confirm `x-noveum-trace-id` / `x-noveum-parent-span-id`
   (UUID) as primary, W3C `traceparent` accepted for interop.
3. **Content capture default:** off (privacy) with opt-in — confirm.
4. **Attribute namespace:** mirror `llm.*` (native in Noveum UI) vs. also
   dual-write OTel `gen_ai.*`. Recommend `llm.*` only.
5. **Project association:** require `NOVEUM_TRACE_PROJECT_ID` (UUID) per deployment
   and/or accept `x-noveum-project-id` per request for multi-tenant.

---
---

# PART II — RESEARCH FINDINGS (APPENDICES)

## Appendix A — `noveum-trace` Python SDK spec

SDK version analyzed: **1.5.17**. Attribute keys are string literals at the
span-creation sites (no central constants file). Native types preserved in
attribute values; nothing forced to JSON except where a site calls `json.dumps`.

**TL;DR for the gateway**
- **No HTTP trace-context propagation exists** — the SDK never reads/writes
  `traceparent`/`tracestate`/`baggage`/`x-*-trace`. The only headers it sets are
  on its *own* export POST: `Authorization: Bearer <key>`,
  `User-Agent: noveum-trace-sdk/<ver>`, `Content-Type`. Cross-process continuity
  is impossible with the SDK as written.
- **Span "type" = span-name prefix** (no `span.kind`): `llm.*`, `tool.*`,
  `agent.*`, `chain.*`, `retrieval.*`, `routing.*`.
- **IDs are UUID4 strings** (not 32-hex/OTLP).
- **Attribute namespace is `llm.*`** (dot-delimited), not `gen_ai.*`. Token usage
  flattened to `llm.input_tokens` / `llm.output_tokens` / `llm.total_tokens`.

**LLM span attributes (manual API + capture_response):** `llm.model`,
`llm.provider`, `llm.operation`; `llm.input.<k>` / `llm.output.<k>` for helper
kwargs; `llm.input_tokens`, `llm.output_tokens`, `llm.total_tokens`;
`llm.cost.input`, `llm.cost.output`, `llm.cost.total`, `llm.cost.currency`
(`"USD"`), legacy `llm.cost`; `llm.context_window`, `llm.max_output_tokens`,
`llm.finish_reason`, `llm.system_fingerprint`, `llm.created`. The response parser
normalizes provider token keys (OpenAI `prompt_tokens`/`completion_tokens`,
Anthropic `input_tokens`/`output_tokens`, Vertex `prompt_token_count`/…, Bedrock,
Watsonx) and can emit OpenAI detail breakdowns (`llm.input_cached_tokens`,
`llm.output_reasoning_tokens`) and Vertex modality keys.

**LangChain auto-instrumentation (richest set — the canonical format):**
`llm.model`, `llm.provider`, `llm.operation` (`"completion"`), `langchain.run_id`;
inputs `llm.input.temperature`, `llm.input.prompts`, `llm.input.messages`,
`llm.input.message_count`, `llm.input.has_tool_calls`, `llm.input.image_count`…;
available tools `llm.available_tools.count`/`.names`/`.descriptions`/`.schemas`;
outputs `llm.output.response`, `llm.output.finish_reason`, `llm.output.tool_calls`,
`llm.output.tool_calls.count`/`.names`, `llm.executed_tool_calls`; usage
`llm.input_tokens`/`output_tokens`/`total_tokens`; cost `llm.cost.*`; timing
`llm.streaming`, `llm.latency_ms`, `llm.first_token_time`,
`llm.time_to_first_token_ms`; custom `noveum.additional_attributes` (JSON string).

**Tool calls:** standalone span name `tool.<name>`; by default LangChain tool
calls are NOT separate spans — buffered + appended onto the parent **LLM** span
with keys `name`, `operation`, `input`, `output`, `status`, `tool_call_id`,
`error`, `called_tool`, `available_tools`, `code_location`, `function_definition`.

**Agent spans:** manual `agent.type`/`agent.operation`/`agent.capabilities`;
`AgentNode` `agent.id`/`type`/`capabilities`/… + rollups; LangChain agent
`agent.name`/`type`/`operation`/`input.inputs`/`output.action.*`/`output.finish.*`;
plus `agent_graph.*` and `agent_workflow.*`.

**Retrieval/RAG spans (LangChain):** `retrieval.type`, `retrieval.operation`,
`retrieval.query`, `retrieval.result_count`, `retrieval.results_truncated`,
`retrieval.sample_results`. **Chain spans:** `chain.name`/`operation`/`inputs`/
`output.*`. **Generic functions:** name = span name; `tag.<k>`, `input.<param>`,
`input.args`, `input.kwargs`. **Streaming:** `streaming.tokens_received`,
`streaming.time_to_first_token`, `streaming.tokens_per_second`, … **Exceptions:**
`exception.type`/`message`/`stacktrace` + an `exception` event.

**Span naming + status:** LangChain `get_operation_name` → `llm.<model>`,
`chain.<name>`, `agent.<name>`, `retrieval.<name>`, `tool.<name>`,
`routing.<src>_to_<tgt>`. Manual → `llm.<operation>` / `llm_call`, etc. Status
enum `unset|ok|error|timeout|cancelled`, serialized as `status` + `status_message`.

**Instrumentation API:** context managers `trace_llm_call`,
`trace_agent_operation`, `trace_operation`, `trace_batch_operation`,
`trace_pipeline_stage`, `trace_context`, `create_child_span`; manual
`start_trace`/`start_span`/`Trace`/`Span`/`NoveumClient`; agents
`create_agent`/`create_agent_graph`/`create_agent_workflow`; streaming +
threads. **No monkey-patching of OpenAI/Anthropic SDKs.** Callback-based
auto-instrumentation only for **LangChain** (`NoveumTraceCallbackHandler`), plus
framework listeners for **CrewAI**, **LiveKit**, **Pipecat**. `IntegrationConfig`
openai/anthropic/langchain/llamaindex flags default disabled + inert.

**Propagation (CRITICAL):** in-process only (contextvars). No header injection on
outbound LLM calls. The only outbound headers are on the SDK's own export POSTs
(+ multipart form fields `traceId`/`spanId` for audio/image uploads, not headers).
Docs mention distributed tracing aspirationally; **no implementation backs it.**

**IDs/assembly/flush:** `trace_id`/`span_id` = `str(uuid.uuid4())` (36-char
UUID4). `parent_span_id` defaults to the current span from contextvars; first span
with `parent_span_id == None` is the root. All spans held in `Trace.spans` and the
whole trace exported as one object on finish. Single → `/v1/trace`; batches →
`/v1/traces` body `{"traces":[...], "timestamp": <epoch>}`; retries on
429/500/502/503/504.

**sdk/project/service:** export injects `sdk = {"name":"noveum-trace-python",
"version":"1.5.17"}`; top-level `service_version`/`project`/`environment` only if
configured. Default endpoint `https://api.noveum.ai/api`; export URL
`endpoint.rstrip('/') + '/v1/traces'`. Env: `NOVEUM_API_KEY`, `NOVEUM_ENDPOINT`,
`NOVEUM_PROJECT`, `NOVEUM_SERVICE_VERSION`, `NOVEUM_ENVIRONMENT`.

## Appendix B — Platform ingest API + gateway telemetry

### B.1 Platform trace-ingest API (`noveum-app-nextjs`)
- Endpoints: `POST /api/v1/traces` (batch `{traces:[…]}`, `BatchTracesRequestSchema`)
  and `POST /api/v1/traces/single` (one Trace, `TraceSchema`). Both wrap to a list
  internally. Source: `packages/api/src/routes/v1/traces/router.ts`,
  schemas `packages/telemetry/src/traces/schemas.ts`.
- Schema as in §3 (trace + span). `name` required (empty → dropped at storage).
  `project` required + must be the project **UUID** (`ensureProjectsExist`
  auto-creates unknown ids as junk projects). `service_version` is a top-level
  sibling, stored on `noveum_traces.service_version` (default `"unknown"`),
  queryable via `?service_version=…` — the `agent-versions` mechanism.
- Auth: `withApiKeyOrSessionAuth` global; Bearer → `db.apiKey.findFirst`; org
  resolved from the key (no org header); ingest needs `traces:write`. Mismatched
  `X-Organization-*` → 403.
- Storage: async via BullMQ → `TraceProcessor` → ClickHouse `noveum_traces` +
  `noveum_spans` (`ReplacingMergeTree`). Org isolation via
  `organization_slug`/`organization_id` columns. **200 = queued, not persisted.**
- Gotchas: re-POST of an existing `trace_id` silently rejected (immutable);
  rate-limit 1000 pts/60s/IP (429); per-org span quota.

### B.2 Gateway telemetry (`/tmp/noveum-ai-gateway`)
- `metrics_middleware` wraps the stack → builds `RequestMetrics` → `record_metrics`
  fans out to each `MetricsExporter` via one `tokio::spawn` (fire-and-forget).
- **`MetricsExporter` trait** (`src/telemetry/metrics.rs`): `async export_metrics
  (RequestMetrics)` + `name()`. `MetricsRegistry::register_exporter` + automatic
  non-blocking fan-out → **the trace exporter's extension point.**
- **`ProviderMetrics`** (`provider_metrics.rs`): `input/output/total_tokens`,
  `cost`, `model`, `provider_latency`, `request_id`, plus `project_id`/
  `organization_id`/`user_id`/`experiment_id`. **No finish_reason field** (lives in
  raw response JSON). Per-provider extractors: OpenAI (`usage.*`), OpenAI-compatible
  (Together/Mistral/Cohere/Google/DeepSeek/xAI/OpenRouter/Perplexity), Anthropic
  (`input/output_tokens`, streaming via thread_local), Bedrock
  (`inputTokens`/`outputTokens` + `x-amzn-RequestId`), Groq (`x_groq.usage`,
  `provider_latency` from `usage.total_time`), Fireworks. Cost via
  `policy::pricing::estimate_cost`.
- `metrics_middleware`: captures provider (`x-provider`), path, method, tracking
  ids (`x-project-id`, `x-organization-id`, `x-user-id`, `x-experiment-id`),
  request/response bodies + sizes, TTFB, `provider_request_id` (`x-request-id`||
  `request-id`); 30s timeout (504 not recorded); streaming detected by
  `content-type: text/event-stream`; **usage IS extracted from streamed chunks**
  (Anthropic/Groq full; OpenAI only with `include_usage`, else `len/4` estimate);
  **no metrics found → nothing recorded → no trace.**
- **No inbound trace context** read today (`traceparent`/`x-noveum-trace`/etc.) —
  the exporter must generate trace/span ids; to support SDK-present, the middleware
  must start reading a propagation header.
- `RequestMetrics` (`telemetry/mod.rs`) carries everything needed for an LLM span,
  incl. full `request_body`/`response_body` + `streamed_data`. A `to_otel_log()`
  already exists (flat, non-OTLP) — we add a `to_noveum_trace()` analog.

### B.3 Mapping (A→B)
`RequestMetrics` → one Noveum trace with a root `llm.<model>` span:
`provider_request_id`→metadata/`request_id`; `model`→`llm.model`; tokens/cost→
`llm.*`; `status_code`→`status`; latency→`duration_ms`; TTFB→
`llm.time_to_first_token_ms`. Gateway tracking headers align: `x-project-id`
should carry the project **UUID**; the gateway's key supplies org. POST to
`/api/v1/traces` (batch), Bearer key, set `service_version`.

## Appendix C — Competitive landscape + propagation

**Per-product proxy telemetry:** LiteLLM (proxy + `success_callback` fan-out to
Langfuse/OTel/Datadog/…; `StandardLoggingPayload`: model, messages, response,
tokens, `response_cost`, `response_time`, cache_hit, status). Helicone (pure proxy,
~10–50ms; prompt+completion, tokens, cost, latency incl. TTFT, cache, errors).
Portkey (OpenAI-compatible gateway; 40+ metrics incl. cost/budgets, retries,
guardrails, custom metadata). Cloudflare AI Gateway (per-request log: model,
tokens, cost+budgets, duration, cache %, status; payloads gated by
`cf-aig-collect-log-payload`). OpenLLMetry/Traceloop (SDK-first OTel GenAI spans;
optional gateway). Universal: model, prompt, response, tokens, cost, latency,
errors, cache.

**Boundary (confirmed):** proxy CAN get model/messages/tokens/latency/
finish_reason/streaming/status + the `tool_calls` *requested* in the response;
CANNOT get tool *execution*/results, agent loops, RAG/retrieval, DB, app spans —
those are in-app spans that never traverse the proxy. ("A proxy sees HTTP but not
internal state.")

**Propagation when an app SDK is present:**
- **W3C `traceparent`** `version-traceid(32hex)-spanid(16hex)-flags` — continuation
  rule: keep the same trace-id, mint a NEW span-id, set parent = incoming span-id.
  LiteLLM honors incoming `traceparent`.
- **Helicone:** `Helicone-Session-Id` (UUID groups a session), `-Session-Path`
  (`/parent/child`), `-Session-Name`. No client-set request id. No W3C support.
- **Portkey:** `x-portkey-trace-id` + `x-portkey-span-id`/`-parent-span-id`/
  `-span-name`; also supports W3C `traceparent`/`baggage` (Portkey headers win).
- **LiteLLM:** body `metadata.trace_id` (new) / `metadata.existing_trace_id`
  (continue); honors `traceparent`.

**OTel `gen_ai.*` (current, still Experimental):** `gen_ai.provider.name` (replaced
`gen_ai.system`, deprecated but still emitted), `gen_ai.operation.name`
(`chat`/`embeddings`/`execute_tool`/`invoke_agent`), `gen_ai.request.model`/
`.temperature`/`.max_tokens`/…, `gen_ai.response.model`/`.id`/`.finish_reasons`,
`gen_ai.usage.input_tokens`/`output_tokens` (renamed from prompt/completion),
content `gen_ai.input.messages`/`output.messages` (opt-in, off by default). Span
name `"{operation} {model}"`.

**Streaming usage:** `stream_options:{include_usage:true}` → extra final chunk with
`usage` + empty `choices[]` just before `[DONE]`; LiteLLM injects it upstream even
if the client didn't, parses it, strips it before forwarding; tokenizer fallback
when absent (mark estimate). Anthropic spreads usage across `message_start`/
`message_delta` (no opt-in).

**Non-blocking emission:** never await telemetry on the request path — enqueue +
background batch-export. OTel `BatchSpanProcessor` is canonical (defaults: 5s
delay, 2048 queue, 512 batch; queue-full = silent drop; ForceFlush on shutdown).
Rust: `opentelemetry_sdk::trace::BatchSpanProcessor`.

**Recommendation (from research):** continue an incoming trace as a **child span**
(never always-new); W3C `traceparent` as the cross-vendor primary for OTel-native
backends — **but** because Noveum traces are UUID-based, a **Noveum-native header**
(`x-noveum-trace-id`) is what actually merges into Noveum traces (see Part I §4).
Mirror the SDK's `llm.*` for Noveum; consider `gen_ai.*` only if exporting to OTel.
