# OpenAI Provider Integration

## Overview

The OpenAI provider forwards OpenAI Chat Completions requests and responses
without changing their JSON shape. Model availability is determined by the
caller's OpenAI project. Nova Guard pricing and strict admission use the
gateway's versioned [pricing catalog](../PRICING.md).

## Model availability and pricing

`gpt-4o-mini`, used below, passed buffered and streaming production probes on
**2026-08-24**. That result is a dated example, not a supported-model inventory:
the caller's OpenAI project and OpenAI's current model lifecycle determine what
it can invoke.

The gateway pricing catalog includes many current and legacy OpenAI rows,
aliases, cache rates, and long-context tiers. Catalog membership means the
gateway can estimate a row; it does not prove upstream availability. An
uncatalogued model is never treated as free: a fail-closed cost cap rejects the
assumption, while other modes use the conservative catalog maximum. Use the
single [pricing guide](../PRICING.md) for exact rates and update rules, and
[OpenAI's pricing reference](https://developers.openai.com/api/docs/pricing) for
the upstream source of truth.

## Configuration

### Request Headers
```bash
Authorization: Bearer sk-...
x-provider: openai
```

## API Endpoints

### Chat Completions
```bash
POST /v1/chat/completions
```

Example request:
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 128,
    "service_tier": "default"
  }'
```

### Streaming
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 128,
    "stream": true,
    "stream_options": {"include_usage": true}
  }'
```

## SDK Integration

### Node.js
```typescript
import OpenAI from 'openai';

const openai = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "openai" }
});

async function getChatCompletion() {
  const response = await openai.chat.completions.create({
    model: "gpt-4o-mini",
    messages: [{ role: "user", content: "Hello!" }],
    max_tokens: 128,
    service_tier: "default"
  });
  console.log(response.choices[0]?.message.content);
}

getChatCompletion();
```

## Error Handling

| Error Code | Description | Solution |
|------------|-------------|----------|
| 401 | Invalid API key | Check your OpenAI API key |
| 429 | Rate limit exceeded | Implement backoff strategy |
| 500 | Server error | Check server logs |

## Best Practices

1. **Rate Limiting**
   - Implement exponential backoff
   - Monitor usage quotas
   - Use streaming for long responses

2. **Error Handling**
   - Implement retry logic
   - Handle timeouts gracefully
   - Log errors appropriately

3. **Performance**
   - Use an explicit output bound. Some reasoning/newer model families require
     `max_completion_tokens`; follow the selected model's current OpenAI contract.
   - Implement caching for repeated requests

When an enforcing/blocking strict Nova Guard cost cap applies, the gateway also
requires a positive output bound and restricts opaque or premium request shapes
that cannot be reserved safely. Direct OpenAI strict traffic is pinned to
`service_tier: "default"`; see [Nova Guard](../NOVA_GUARD.md).

## Monitoring

The native telemetry record includes latency, TTFB, status, provider status,
token usage, cost, and pricing completeness. See
[Telemetry and log handling](../logs.md). For streaming settlement, request a
terminal usage frame with `stream_options.include_usage`.
