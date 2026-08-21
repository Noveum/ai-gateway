# OpenAI Provider Integration

## Overview

The OpenAI provider forwards OpenAI Chat Completions requests and responses
without changing their JSON shape. Model availability is determined by the
caller's OpenAI project. Nova Guard pricing and strict admission use the
gateway's versioned [pricing catalog](../PRICING.md).

## Supported Models

### Current catalog rows

- `gpt-5.6-luna` — $0.20/M input, $1.20/M output at standard short-context rates
- `gpt-5.6-terra` — $2/M input, $12/M output
- `gpt-5.6-sol` — $5/M input, $30/M output
- `gpt-5.6-cyber` — $12.50/M input, $75/M output
- `gpt-5.6` and `daybreak-blue-latest` resolve to Sol for pricing;
  `daybreak-red-latest` resolves to Cyber

The catalog also contains the supported GPT-4.x, reasoning, embedding, image,
and audio rows listed in [Pricing](../PRICING.md). A model that OpenAI still
accepts but the current catalog does not price is never treated as free: strict
fail-closed cost caps reject it, while other modes use the conservative catalog
maximum. See [OpenAI's current pricing](https://developers.openai.com/api/docs/pricing)
for the upstream source of truth.

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
    "model": "gpt-5.6-luna",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_completion_tokens": 128,
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
    "model": "gpt-5.6-luna",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_completion_tokens": 128,
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
    model: "gpt-5.6-luna",
    messages: [{ role: "user", content: "Hello!" }],
    max_completion_tokens: 128,
    service_tier: "default"
  });
  console.log(response.data);
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
   - Use an explicit output bound. GPT-5.6 uses `max_completion_tokens`; the
     legacy `max_tokens` field is rejected by the upstream API for these models.
   - Batch requests when possible
   - Implement caching for repeated requests

When an enforcing/blocking strict Nova Guard cost cap applies, the gateway also
requires a positive output bound and restricts opaque or premium request shapes
that cannot be reserved safely. Direct OpenAI strict traffic is pinned to
`service_tier: "default"`; see [Nova Guard](../NOVA_GUARD.md).

## Monitoring

### Available Metrics (Coming Soon)
- Request latency
- Token usage
- Error rates
- Request volume
