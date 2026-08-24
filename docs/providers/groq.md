# GROQ Provider Integration

## Overview

Groq provider support routes the gateway's OpenAI Chat Completions interface to
GroqCloud. Request capabilities such as vision, reasoning, structured output,
and tool use depend on the selected Groq model; the gateway does not add a
capability the upstream model lacks.

## Model availability

The runnable examples and integration tests use `openai/gpt-oss-20b`, a Groq
production model with a 131,072-token context window and 65,536 maximum output
tokens. Groq changes production and preview availability independently of this
gateway. Do not copy a frozen list from this repository; use Groq's
[current model table](https://console.groq.com/docs/models) and
[deprecation history](https://console.groq.com/docs/deprecations), or query
Groq's authenticated `/openai/v1/models` endpoint.

Groq retired `llama-3.1-8b-instant` for affected plans on August 16, 2026 and
names `openai/gpt-oss-20b` as its replacement. The examples below therefore do
not use the older Llama, Mixtral, or preview slugs.

## Configuration

### Request Headers
```bash
Authorization: Bearer $GROQ_API_KEY
x-provider: groq
```

## API Examples

### Chat Completions (cURL)
```bash
curl http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $GROQ_API_KEY" \
  -H "x-provider: groq" \
  -d '{
    "model": "openai/gpt-oss-20b",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 500
  }'
```

### Vision requests

The gateway forwards OpenAI `image_url` content, but the model ID must be one
Groq currently marks as vision-capable. Check the live model table before using
one; preview vision slugs in older versions of this guide have been retired.

### Tool Use Example (cURL)
```bash
curl --location 'localhost:3000/v1/chat/completions' \
--header 'Authorization: Bearer YOUR_GROQ_KEY' \
--header 'Content-Type: application/json' \
--header 'x-provider: groq' \
--data '{
    "model": "openai/gpt-oss-20b",
    "messages": [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "What'\''s weather in Bangalore?"
                }
            ]
        }
    ],
    "tools": [
        {
            "type": "function",
            "function": {
                "name": "get_current_weather",
                "description": "Get the current weather in a given location",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {
                            "type": "string",
                            "description": "The city and state, e.g. San Francisco, CA"
                        },
                        "unit": {
                            "type": "string",
                            "enum": [
                                "celsius",
                                "fahrenheit"
                            ]
                        }
                    },
                    "required": [
                        "location"
                    ]
                }
            }
        }
    ],
    "tool_choice": "auto",
    "stream": false,
    "max_tokens": 300
}'
```

### OpenAI SDK Integration
```typescript
import OpenAI from 'openai';

const client = new OpenAI({
  apiKey: process.env.GROQ_API_KEY,
  baseURL: "http://localhost:3000/v1",
  defaultHeaders: { "x-provider": "groq" }
});

async function main() {
  const completion = await client.chat.completions.create({
    model: "openai/gpt-oss-20b",
    messages: [{ role: "user", content: "Hello!" }],
    max_tokens: 500,
    stream: false
  });
  
  console.log(completion.choices[0].message);
}
```

### Streaming Example
```typescript
const stream = await client.chat.completions.create({
  model: "openai/gpt-oss-20b",
  messages: [{ role: "user", content: "Hello!" }],
  stream: true
});

for await (const chunk of stream) {
  process.stdout.write(chunk.choices[0]?.delta?.content || '');
}
```

## Response Format
```json
{
  "id": "chatcmpl-f51b2cd2-bef7-417e-964e-a08f0b513c22",
  "object": "chat.completion",
  "created": 1730241104,
  "model": "openai/gpt-oss-20b",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "Hello! How can I help you today?"
    },
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 18,
    "completion_tokens": 56,
    "total_tokens": 74
  }
}
```

## Error Handling

| Error Code | Description | Solution |
|------------|-------------|----------|
| 401 | Invalid API key | Verify your GROQ API key |
| 429 | Rate limit exceeded | Implement backoff strategy |
| 500 | Server error | Check server logs |

## Provider Implementation
The GROQ provider is implemented in `src/providers/groq.rs`. It handles authentication validation and request processing through the Provider trait implementation:

```rust
impl Provider for GroqProvider {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn name(&self) -> &str {
        "groq"
    }

    fn process_headers(&self, original_headers: &HeaderMap) -> Result<HeaderMap, AppError> {
        // Header processing implementation
    }
}
```
