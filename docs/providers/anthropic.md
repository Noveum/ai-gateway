# Anthropic through the OpenAI Chat Completions interface

The gateway exposes Anthropic through **`POST /v1/chat/completions` with an
OpenAI-shaped request and response**. It translates the request to Anthropic's
`POST /v1/messages` API, then translates successful buffered and streaming
responses back to OpenAI Chat Completions format. The native gateway and the
Cloudflare Worker use the same request, buffered-response, and SSE translation
code.

This is a compatibility adapter, not a native Anthropic Messages endpoint. Do
not point the Anthropic SDK at the gateway: even if a native-looking request is
accepted, a successful response is OpenAI-shaped and will not satisfy the
Anthropic SDK's response contract. Use an OpenAI SDK or direct HTTP.

## Configuration and authentication

Send `x-provider: anthropic` and authenticate in either form:

```http
Authorization: Bearer sk-ant-...
```

or:

```http
x-api-key: sk-ant-...
```

The gateway sends `x-api-key` upstream and sets
`anthropic-version: 2023-06-01`. It forwards `anthropic-beta`, tracing headers,
other non-internal custom headers, and the query string. Gateway-internal
headers such as `x-provider`, Noveum tenancy headers, and `x-aws-*` are removed.
If both supported credential headers are supplied, the Bearer credential takes
precedence and becomes the upstream `x-api-key`.

`ANTHROPIC_BASE_URL` overrides the default `https://api.anthropic.com` upstream
on both runtimes. The gateway appends `/v1/messages`; use this only for a
compatible proxy or a test server.

## Current models and pricing

Examples below use `claude-sonnet-5`, an active Claude API model ID in
[Anthropic's current model table](https://platform.claude.com/docs/en/about-claude/models/overview).
The gateway does not restrict requests to this list, but Nova Guard catalog
version `2026.08.21` includes these current high-value rows:

| Model | Availability note | Input | Output | Cache hit | 5m write | 1h write |
|---|---|---:|---:|---:|---:|---:|
| `claude-sonnet-5` | Active; $2/$10 launch pricing is permanent | $2.00 | $10.00 | $0.20 | $2.50 | $4.00 |
| `claude-opus-5` | Active | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-opus-4-8` | Active | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-opus-4-5-20251101` | Active dated model ID | $5.00 | $25.00 | $0.50 | $6.25 | $10.00 |
| `claude-fable-5` | Active | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |
| `claude-mythos-5` | Active, limited/invitation-only availability | $10.00 | $50.00 | $1.00 | $12.50 | $20.00 |

All values are USD per million tokens. Anthropic made Sonnet 5's $2/$10
launch pricing permanent, so the catalog deliberately has no scheduled $3/$15
increase. Model availability and IDs change independently of gateway releases;
verify them against the
[model table](https://platform.claude.com/docs/en/about-claude/models/overview)
and [deprecation table](https://platform.claude.com/docs/en/about-claude/model-deprecations).

These rates are policy estimates, not an invoice. Nova Guard models cache,
US-only inference, and supported fast-mode premiums; batch discounts,
negotiated pricing, taxes, and later provider changes can still differ. See the
[pricing and accounting guide](../PRICING.md) and verify time-sensitive rates
against [Anthropic's pricing page](https://platform.claude.com/docs/en/about-claude/pricing).

For a simple uncached call with 1,000 input and 500 output tokens, the catalog
estimate is `(1,000 / 1,000,000 × $2) + (500 / 1,000,000 × $10) = $0.007`.

## Examples

### Buffered chat completion

```bash
curl http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "Authorization: Bearer $ANTHROPIC_API_KEY" \
  -d '{
    "model": "claude-sonnet-5",
    "messages": [
      {"role": "system", "content": "Be concise."},
      {"role": "user", "content": "Explain atomic admission in one sentence."}
    ],
    "max_tokens": 128
  }'
```

### Streaming

```bash
curl -N http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -d '{
    "model": "claude-sonnet-5",
    "messages": [{"role": "user", "content": "Count to three."}],
    "max_completion_tokens": 64,
    "stream": true
  }'
```

The client receives OpenAI `chat.completion.chunk` frames followed by
`data: [DONE]`; raw Anthropic event names do not leak through a successful
stream. The final chunk carries translated prompt, completion, and total token
usage.

### OpenAI TypeScript SDK

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.ANTHROPIC_API_KEY,
  baseURL: "http://localhost:3000/v1",
  defaultHeaders: { "x-provider": "anthropic" },
});

const response = await client.chat.completions.create({
  model: "claude-sonnet-5",
  messages: [{ role: "user", content: "Hello!" }],
  max_tokens: 128,
});

console.log(response.choices[0].message.content);
```

### Function tool call

```json
{
  "model": "claude-sonnet-5",
  "messages": [{"role": "user", "content": "What is the weather in Paris?"}],
  "max_output_tokens": 256,
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Return the current weather for a city",
        "parameters": {
          "type": "object",
          "properties": {"city": {"type": "string"}},
          "required": ["city"]
        }
      }
    }
  ],
  "tool_choice": "auto"
}
```

Buffered Anthropic `tool_use` blocks become OpenAI `message.tool_calls`.
Streaming `tool_use` plus fragmented `input_json_delta` events become indexed
OpenAI `delta.tool_calls` fragments. To continue a tool conversation, send the
assistant's `tool_calls`, followed by one `role: "tool"` result for every pending
`tool_call_id` before the next non-tool message.

This is **client function-tool** compatibility. Anthropic-managed server tools
(web search, web fetch, code execution, computer use, and MCP tool definitions)
are not accepted request shapes by this adapter. Use Anthropic's native API
when those features are required.

## Nova Guard preflight and Anthropic premiums

The request is translated and validated before either the native gateway or
Worker creates a Nova Guard reservation. A validation failure is HTTP 400 and
does not call Anthropic.

- `cache_control` is validated at supported top-level, system-block,
  message-content, and tool-definition locations. Every value must be an object
  with `type: "ephemeral"`; `ttl` may be omitted (5 minutes), `"5m"`, or
  `"1h"`. Native Anthropic-shaped blocks and client tools preserve the field.
  The current OpenAI `function`-tool and `image_url` mappings rebuild those
  objects and do **not** carry a nested `cache_control` through; use a preserved
  native-shaped location for that breakpoint. Strict admission still sees a
  supplied declaration and reserves the entire estimated prompt as a
  first-write miss at the longest declared TTL, including the 1-hour 2x write
  premium. Settlement uses Anthropic's actual cache-read and cache-creation
  counters.
- `inference_geo` must be `"global"` or `"us"` and the model must support it.
  Global uses standard rates; US multiplies every token/cache dimension by
  1.1. If an eligible request omits the field, admission conservatively
  reserves 1.1x because the workspace can choose US-only inference. Buffered
  and streaming usage preserve Anthropic's reported geo so settlement can
  reconcile to the exact multiplier.
- `speed: "standard"` is accepted. `speed: "fast"` is accepted only for the
  `claude-opus-5` and `claude-opus-4-8` families and multiplies every
  token/cache dimension by 2. Fast plus US-only inference stacks
  multiplicatively to 2.2x in both admission and settlement. An invalid speed,
  or fast mode on another model, is rejected before admission. Anthropic exposes
  fast mode as a research preview, so the caller must also send
  `anthropic-beta: fast-mode-2026-02-01`; the gateway forwards that header but
  does not add it implicitly.
- `fallbacks` is always rejected because Anthropic can bill multiple models in
  one response while the current reservation and settlement record has one
  model.
- On constrained Claude families (Opus 4.7/4.8/5, Sonnet 5, Fable 5, and
  Mythos 5/preview), `temperature` may be omitted or exactly `1`, `top_p` may be
  omitted or be from `0.99` through `1`, and `top_k` must be omitted. Other
  values are rejected before admission.
- Sonnet 5 rejects manual `thinking.type: "enabled"`; use `adaptive` or
  `disabled` thinking. It also rejects a final assistant-message prefill.

An Anthropic pre-output refusal reports `finish_reason: "content_filter"` to
the OpenAI client. When Anthropic reports zero output tokens, Nova Guard keeps
the input/cache counts for observability but settles monetary cost to $0. A
refusal after any generated output is billed normally.

## Request compatibility

The adapter intentionally supports a defined subset of OpenAI Chat Completions.
Anthropic also describes its own OpenAI compatibility trade-offs in the
[official compatibility guide](https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk);
the table below describes **this gateway's implementation**.

| Input | Gateway behavior |
|---|---|
| `model`, `stream` | Forwarded to Anthropic |
| `max_tokens`, `max_completion_tokens`, `max_output_tokens` | First non-null value wins and becomes Anthropic `max_tokens`; it must be a positive integer. If all are absent and no applicable strict Nova Guard cap requires a real bound, the gateway supplies `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` (default `1024`) |
| `temperature` | For constrained families, omit it or use exactly `1`. For other models it must be non-negative; values above `1` are capped at `1` |
| `top_p`, `top_k` | Forwarded for models that accept them. Constrained families require omitted or `0.99`–`1` `top_p` and reject any `top_k` |
| `stop` | String or string array converted to `stop_sequences`, unless `stop_sequences` is already present |
| `system` plus `system` / `developer` messages | Text is hoisted and concatenated in encounter order into Anthropic's top-level system prompt |
| `n` | Must be exactly `1` |
| OpenAI function `tools` | `function.name`, `description`, and `parameters` map to Anthropic `name`, `description`, and `input_schema`; an outer `cache_control` is not carried through this mapping |
| `tool_choice` | `auto`, `required`, `none`, and a named OpenAI function choice map to Anthropic `auto`, `any`, `none`, and `tool` |
| `parallel_tool_calls: false` | Adds Anthropic `disable_parallel_tool_use: true`; `true` keeps Anthropic's normal parallel behavior |
| Assistant `tool_calls` and `role: "tool"` results | Converted to `tool_use` and `tool_result`, with ordering and ID validation |
| User `image_url` content | Supports HTTP(S), or base64 data URLs for JPEG, PNG, GIF, and WebP; a `cache_control` on the OpenAI image part is not carried through the image mapping |
| `cache_control` | Validated as `type: "ephemeral"` with an omitted, `5m`, or `1h` TTL. It is forwarded on preserved native Anthropic-shaped locations; the two OpenAI mappings called out above drop it. A supplied declaration is still included in Nova Guard reservation |
| `inference_geo` | `global` or `us` on eligible models; `us` is priced at 1.1x and omitted eligible requests reserve 1.1x conservatively |
| `speed` | `standard` is forwarded; `fast` is limited to Opus 5/4.8 and priced at 2x, multiplicative with the geo premium. The caller must include Anthropic's `fast-mode-2026-02-01` beta header |
| `fallbacks` | Rejected before admission; mixed-model usage cannot yet be represented by one Nova Guard settlement record |
| `mcp_servers` and provider-managed/server `tools` | Rejected before admission in every policy mode. Only OpenAI `type: "function"` tools and untyped native Anthropic client tools are accepted |
| Native Anthropic text/image/tool/thinking blocks inside compatible messages | Preserved when their role and shape are valid, but native-only response blocks are not exposed in the OpenAI response |
| Native Anthropic `stop_sequences`, thinking, system blocks, client tools, and tool-choice objects | Forwarded after the validations and OpenAI mappings above; constrained-family sampling and Sonnet 5 thinking/prefill restrictions still apply |

The following OpenAI fields are removed before the Anthropic request because
the gateway does not implement an equivalent:

`response_format`, `frequency_penalty`, `presence_penalty`, `logprobs`,
`top_logprobs`, `seed`, `service_tier`, `reasoning_effort`, `store`, `user`,
`modalities`, `audio`, `prediction`, `logit_bias`, `web_search_options`,
`verbosity`, `prompt_cache_key`, `safety_identifier`, and `metadata`.

`stream_options` is also removed; for a successful Anthropic stream the gateway
itself emits the terminal OpenAI usage chunk. The legacy `functions` and
`function_call` request fields, including message-level `function_call`, are
rejected with HTTP 400. Use `tools`, `tool_choice`, and `tool_calls` instead.
The OpenAI function-tool `strict` flag is not forwarded, so this adapter does
not promise schema-conformant tool arguments.

Within each message, OpenAI-only `name`, `refusal`, and `audio` properties are
removed. Except for explicitly rejected fields such as `fallbacks` and
`mcp_servers`, unknown top-level request properties are currently passed through
for Anthropic to validate; that pass-through is not a compatibility guarantee,
and clients should rely only on the translated or native-extension fields
listed above.

Audio, PDF/file content, citations, and native server/MCP tools have no complete
OpenAI Chat Completions representation here. User audio/file parts and native
server/MCP tool definitions are rejected. If an upstream response nevertheless
contains server/MCP, thinking/signature, or citation blocks, those blocks are
omitted rather than misreported as assistant text; reported server-tool usage
dimensions remain available to metering. Use Anthropic's native API directly
when those features are required.

## Response and error contract

| Upstream outcome | Client receives |
|---|---|
| Buffered Anthropic 2xx Message | OpenAI `chat.completion` with one choice, translated text/tool calls, finish reason, and the usage fields Anthropic supplied; available cache/server-tool, `inference_geo`, and `speed` dimensions remain as additional usage keys |
| Anthropic 2xx SSE stream | OpenAI `chat.completion.chunk` SSE, translated text/tool-call deltas, available terminal usage (including cache/server-tool, `inference_geo`, and `speed` dimensions), then `[DONE]` |
| Truncated, malformed, or upstream-error SSE | An in-band `event: error` frame and an aborted stream; no false clean `[DONE]` |
| Anthropic 4xx/5xx | Original HTTP status and Anthropic error envelope, not an OpenAI error rewrite |
| Invalid compatibility input | Gateway HTTP 400 `invalid_request_error` |
| Applicable enforcing/blocking strict Nova Guard cost cap without a positive explicit output limit | Gateway HTTP 400 with `error.code: "missing_output_limit"`, before admission or provider dispatch |

Output-phase Nova Guard enforcement is skipped for all streaming providers in
the current release; input-phase policies still run. For platform-managed
metering, translated terminal usage is used to settle the reservation only when
Anthropic supplied both input and output token counts. The buffered and stream
converters preserve missing or partial usage as missing; they never manufacture
`0/0`. Nova Guard therefore abandons the reservation and retains its
conservative estimate when either authoritative half is unavailable. An
explicit reported zero remains authoritative.

## Primary references

- [Claude model overview and current IDs](https://platform.claude.com/docs/en/about-claude/models/overview)
- [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [Sonnet 5 behavior changes](https://platform.claude.com/docs/en/about-claude/models/whats-new-sonnet-5)
- [Messages API](https://platform.claude.com/docs/en/api/messages/create)
- [Fast mode](https://platform.claude.com/docs/en/build-with-claude/fast-mode)
- [Refusals and server-side fallback](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)
- [Streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview)
- [Vision and supported image formats](https://platform.claude.com/docs/en/build-with-claude/vision)
- [Anthropic's OpenAI SDK compatibility notes](https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk)
