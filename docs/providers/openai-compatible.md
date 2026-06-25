# OpenAI-Compatible Providers

Several providers expose an OpenAI-compatible Chat Completions API (same request
and response shape, Bearer auth). Noveum AI Gateway serves all of them through a
single generic adapter (`OpenAICompatibleProvider`,
`src/providers/openai_compatible.rs`) — the only per-provider differences are the
upstream base URL and an optional `/v1` path rewrite.

To use any of them: send your request to the gateway's `/v1/chat/completions`
endpoint with `x-provider: <name>` and `Authorization: Bearer <that provider's
API key>`. Token usage and per-request cost are extracted from the standard
`usage` object and priced via the shared table (see [PRICING.md](../PRICING.md)).

## Providers

| `x-provider` | Upstream endpoint | Auth | Model list |
|---|---|---|---|
| `mistral` | `https://api.mistral.ai/v1` | `Bearer $MISTRAL_API_KEY` | https://docs.mistral.ai/getting-started/models/ |
| `cohere` | `https://api.cohere.ai/compatibility/v1` | `Bearer $COHERE_API_KEY` | https://docs.cohere.com/docs/models |
| `google` / `gemini` | `https://generativelanguage.googleapis.com/v1beta/openai` | `Bearer $GEMINI_API_KEY` | https://ai.google.dev/gemini-api/docs/models |
| `deepseek` | `https://api.deepseek.com/v1` | `Bearer $DEEPSEEK_API_KEY` | https://api-docs.deepseek.com/quick_start/pricing |
| `xai` / `grok` | `https://api.x.ai/v1` | `Bearer $XAI_API_KEY` | https://docs.x.ai/docs/models |
| `openrouter` | `https://openrouter.ai/api/v1` | `Bearer $OPENROUTER_API_KEY` | https://openrouter.ai/models |
| `perplexity` | `https://api.perplexity.ai` | `Bearer $PERPLEXITY_API_KEY` | https://docs.perplexity.ai/guides/model-cards |

> Pricing note: OpenRouter is a meta-router; the gateway prices a request only
> when the upstream model id it returns is present in the pricing table.
> Perplexity's per-request search fees are billed by Perplexity and are not
> represented in the token cost.

## Example (cURL)

```bash
# Google Gemini via the OpenAI-compatible endpoint
curl http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: google" \
  -H "Authorization: Bearer $GEMINI_API_KEY" \
  -d '{
    "model": "gemini-2.5-flash",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 200
  }'
```

Switch provider by changing `x-provider`, the API key, and the `model`:

```bash
# DeepSeek
curl http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: deepseek" \
  -H "Authorization: Bearer $DEEPSEEK_API_KEY" \
  -d '{"model": "deepseek-chat", "messages": [{"role": "user", "content": "Hello!"}]}'
```

## Example (OpenAI SDK)

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.XAI_API_KEY,
  baseURL: "http://localhost:3000/v1",
  defaultHeaders: { "x-provider": "xai" },
});

const completion = await client.chat.completions.create({
  model: "grok-4.3",
  messages: [{ role: "user", content: "Hello!" }],
});
console.log(completion.choices[0].message);
```

## Project / cost attribution

Add the tracking headers to tag the per-request metrics: `x-project-id`,
`x-organization-id`, `x-user-id`, `x-experiment-id`.

## Implementation

All of these are constructed by `openai_compatible(name)` in
`src/providers/mod.rs` and served by `OpenAICompatibleProvider`. Adding another
OpenAI-compatible vendor is a one-line addition there (name, base URL, and
whether to strip the leading `/v1`).
