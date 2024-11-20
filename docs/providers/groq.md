# GROQ Provider Integration

## Overview
GROQ provider support in MagicAPI AI Gateway enables high-performance access to GROQ's inference infrastructure through an OpenAI-compatible API interface. This API also supports MultiModal vision models and tool use.

## Supported Models

| Model ID                               | Developer   | Context Window (tokens) | Max Tokens |
|----------------------------------------|-------------|-------------------------|------------|
| gemma2-9b-it                           | Google      | 8,192                   | -          |
| gemma-7b-it                            | Google      | 8,192                   | -          |
| llama3-groq-70b-8192-tool-use-preview  | Groq        | 8,192                   | -          |
| llama3-groq-8b-8192-tool-use-preview   | Groq        | 8,192                   | -          |
| llama-3.1-70b-versatile                | Meta        | 128,000                 | 32,768     |
| llama-3.1-70b-specdec                  | Meta        | 128,000                 | 8,192      |
| llama-3.1-8b-instant                   | Meta        | 128,000                 | 8,192      |
| llama-3.2-1b-preview                   | Meta        | 128,000                 | 8,192      |
| llama-3.2-3b-preview                   | Meta        | 128,000                 | 8,192      |
| llama-3.2-11b-vision-preview           | Meta        | 128,000                 | 8,192      |
| llama-3.2-90b-vision-preview           | Meta        | 128,000                 | 8,192      |
| llama-guard-3-8b                       | Meta        | 8,192                   | -          |
| llama3-70b-8192                        | Meta        | 8,192                   | -          |
| llama3-8b-8192                         | Meta        | 8,192                   | -          |
| mixtral-8x7b-32768                     | Mistral     | 32,768                  | -          |

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
    "model": "mixtral-8x7b-32768",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "max_tokens": 500
  }'
```

### MultiModal Vision Model Example (cURL)
```bash
curl --location 'localhost:3000/v1/chat/completions' \
--header 'Authorization: Bearer $GROQ_API_KEY' \
--header 'Content-Type: application/json' \
--header 'x-provider: groq' \
--data '{
    "model": "llama-3.2-11b-vision-preview",
    "messages": [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "What'\''s in this image?"
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://upload.wikimedia.org/wikipedia/commons/f/f2/LPU-v1-die.jpg"
                    }
                }
            ]
        }
    ],
    "stream": false,
    "max_tokens": 300
}'
```

### Tool Use Example (cURL)
```bash
curl --location 'localhost:3000/v1/chat/completions' \
--header 'Authorization: Bearer YOUR_GROQ_KEY' \
--header 'Content-Type: application/json' \
--header 'x-provider: groq' \
--data '{
    "model": "llama-3.2-11b-vision-preview",
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
    model: "mixtral-8x7b-32768",
    messages: [{ role: "user", content: "Hello!" }],
    temperature: 0.7,
    max_tokens: 500,
    stream: false
  });
  
  console.log(completion.choices[0].message);
}
```

### Streaming Example
```typescript
const stream = await client.chat.completions.create({
  model: "mixtral-8x7b-32768",
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
  "model": "mixtral-8x7b-32768",
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
    "completion_tokens": 556,
    "total_tokens": 574
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