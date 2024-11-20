# OpenAI Provider Integration

## Overview
The OpenAI provider supports a diverse set of models with different capabilities and price points. You can also make customizations to our models for your specific use case with fine-tuning.

## Supported Models

### Models Overview
- **GPT-4o**: Our high-intelligence flagship model for complex, multi-step tasks
- **GPT-4o mini**: Our affordable and intelligent small model for fast, lightweight tasks
- **o1-preview and o1-mini**: Language models trained with reinforcement learning to perform complex reasoning.
- **GPT-4 Turbo and GPT-4**: The previous set of high-intelligence models
- **GPT-3.5 Turbo**: A fast, inexpensive model for simple tasks
- **Embeddings**: A set of models that can convert text into a numerical form
- **Moderation**: A fine-tuned model that can detect whether text may be sensitive or unsafe

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
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7
  }'
```

### Streaming
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
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
    model: "gpt-4o",
    messages: [{ role: "user", content: "Hello!" }]
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
   - Use appropriate max_tokens
   - Batch requests when possible
   - Implement caching for repeated requests

## Monitoring

### Available Metrics (Coming Soon)
- Request latency
- Token usage
- Error rates
- Request volume
