# Fireworks AI provider

The `fireworks` route forwards OpenAI Chat Completions requests to
`https://api.fireworks.ai/inference/v1`. It preserves buffered/SSE response
shapes, the upstream `x-request-id`, and standard token usage for telemetry and
Nova Guard settlement.

## Authentication

```text
x-provider: fireworks
Authorization: Bearer <Fireworks API key>
```

The key is supplied on each request and forwarded only to Fireworks. Do not put
it in gateway configuration, Worker secrets, source control, or logs.

## Buffered request

This model ID was verified through the production gateway on **2026-08-24**:

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $FIREWORKS_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: fireworks" \
  -d '{
    "model": "accounts/fireworks/models/deepseek-v4-flash-0731",
    "messages": [{"role": "user", "content": "Reply with one sentence."}],
    "max_tokens": 64
  }'
```

## Streaming request

```bash
curl --no-buffer --fail-with-body \
  http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $FIREWORKS_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: fireworks" \
  -d '{
    "model": "accounts/fireworks/models/deepseek-v4-flash-0731",
    "messages": [{"role": "user", "content": "Count from one to three."}],
    "max_tokens": 64,
    "stream": true
  }'
```

The dated ID is a verified example, not an evergreen inventory. Fireworks can
add, rename, or retire models independently of this gateway. The older
`llama-v3p1-*` examples and the catalogued
`accounts/fireworks/models/llama-v3p3-70b-instruct` returned upstream 404s in
the same 2026-08-24 audit and are intentionally not presented as runnable.
That smoke observed provider-emitted terminal usage without adding OpenAI's
undocumented-for-Fireworks `stream_options.include_usage` request field. Check
the [Fireworks model library](https://fireworks.ai/models) before choosing a
model.

## OpenAI SDK

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.FIREWORKS_API_KEY,
  baseURL: "http://127.0.0.1:3000/v1",
  defaultHeaders: { "x-provider": "fireworks" },
});

const completion = await client.chat.completions.create({
  model: "accounts/fireworks/models/deepseek-v4-flash-0731",
  messages: [{ role: "user", content: "Reply with one sentence." }],
  max_tokens: 64,
});
```

## Nova Guard and pricing

- The gateway reads the standard Fireworks `usage.prompt_tokens`,
  `completion_tokens`, and `total_tokens` fields.
- Catalog family matching lets a dated suffix such as `-0731` use the reviewed
  `accounts/fireworks/models/deepseek-v4-flash` pricing row when the boundary
  match is valid.
- A model absent from the catalog is conservatively estimated, not treated as
  free. A fail-closed cost cap can reject an assumed model price.
- An applicable strict cost cap requires a positive explicit output limit and
  rejects request shapes that cannot be bounded before admission.

Pricing is a policy estimate, not the Fireworks invoice. See
[Pricing and cost accounting](../PRICING.md).

## Troubleshooting

| Symptom | Check |
|---|---|
| 401/403 | The per-request Fireworks key and its account permissions. |
| 404 | The exact live model ID; do not assume a catalog row proves upstream availability. |
| 429 | Fireworks account/model limits and client retry/backoff. |
| Stream has no authoritative usage | Treat it as a provider/model behavior change, record the smoke as incomplete, and expect Nova Guard to retain its conservative estimate. |

## Primary references

- [Fireworks documentation](https://docs.fireworks.ai/)
- [Fireworks model library](https://fireworks.ai/models)
- [Fireworks pricing](https://fireworks.ai/pricing)
