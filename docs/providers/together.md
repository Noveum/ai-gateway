# Together AI provider

The `together` route forwards the gateway's OpenAI Chat Completions interface
to `https://api.together.xyz`. Buffered and SSE responses remain
OpenAI-compatible. If Together does not return an `x-request-id`, the gateway
uses the response ID or generates a local request ID for tracing.

## Authentication

```text
x-provider: together
Authorization: Bearer <Together API key>
```

Supply the provider key per request. It is not a Noveum API key and should not
be stored in gateway/Worker configuration.

## Buffered request

This model ID was verified through the production gateway on **2026-08-24**:

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $TOGETHER_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -d '{
    "model": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "messages": [{"role": "user", "content": "Reply with one sentence."}],
    "max_tokens": 64
  }'
```

## Streaming request

```bash
curl --no-buffer --fail-with-body \
  http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $TOGETHER_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -d '{
    "model": "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "messages": [{"role": "user", "content": "Count from one to three."}],
    "max_tokens": 64,
    "stream": true
  }'
```

Together changes serverless/dedicated model availability independently of the
gateway. The example is a dated production proof, not a frozen supported-model
list; the 2026-08-24 smoke observed provider-emitted terminal usage without
adding OpenAI's undocumented-for-Together `stream_options.include_usage`
request field. Query Together's models endpoint or use its
[current model reference](https://docs.together.ai/reference/models) before
deployment.

## OpenAI SDK

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.TOGETHER_API_KEY,
  baseURL: "http://127.0.0.1:3000/v1",
  defaultHeaders: { "x-provider": "together" },
});

const completion = await client.chat.completions.create({
  model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
  messages: [{ role: "user", content: "Reply with one sentence." }],
  max_tokens: 64,
});
```

## Nova Guard and pricing

The gateway reads Together's standard token usage and prices a case-insensitive
model match against the versioned catalog. Model availability and price support
are separate questions: a provider can serve a model that this catalog does not
yet know, and a legacy catalog row does not prove the provider still serves it.
Unknown models use a conservative estimate and can be rejected by a fail-closed
cost cap.

An applicable strict cost cap requires a positive explicit output limit and
rejects unbounded provider-side work before admission. See
[Nova Guard](../NOVA_GUARD.md) and [Pricing](../PRICING.md).

## Troubleshooting

| Symptom | Check |
|---|---|
| 401/403 | The Together key and account permissions. |
| 404 | The exact current model ID and whether it is serverless or dedicated. |
| 429 | Together account/model limits and client retry/backoff. |
| Missing streaming usage | Treat it as a provider/model behavior change, record the smoke as incomplete, and expect Nova Guard to retain its conservative estimate. |

## Primary references

- [Together AI documentation](https://docs.together.ai/)
- [Together model reference](https://docs.together.ai/reference/models)
- [Together pricing](https://www.together.ai/pricing)
