# Anthropic Provider Integration

## Overview
The Anthropic provider integrates with Claude models through Anthropic's API, with automatic path transformation for OpenAI-compatible endpoints.

## Configuration

### Headers
```bash
Authorization: Bearer sk-ant-...
x-provider: anthropic
```

## API Compatibility

The provider automatically transforms OpenAI-compatible requests to Anthropic's format. Below is a code snippet from the `anthropic.rs` file that shows how the path transformation is handled:

```rust:src/providers/anthropic.rs
impl AnthropicProvider {
    // ... existing code ...

    fn transform_path(&self, path: &str) -> String {
        if path.contains("/chat/completions") {
            "/v1/messages".to_string()
        } else {
            path.to_string()
        }
    }

    // ... existing code ...
}
```

## Supported Models
Last updated: November 19, 2024

### Claude 3.5 Series
- claude-3-5-sonnet-20241022 (alias: claude-3-5-sonnet-latest)
- claude-3-5-haiku-20241022 (alias: claude-3-5-haiku-latest)

### Claude 3 Series
- claude-3-opus-20240229 (alias: claude-3-opus-latest)
- claude-3-sonnet-20240229
- claude-3-haiku-20240307

## Examples

### Basic Chat Completion
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "Authorization: Bearer $ANTHROPIC_API_KEY" \
  -d '{
    "model": "claude-3-opus-20240229",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Streaming Response
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "Authorization: Bearer $ANTHROPIC_API_KEY" \
  -d '{
    "model": "claude-3-opus-20240229",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'
```

### OpenAI SDK Compatibility
```typescript
import OpenAI from 'openai';

const openai = new OpenAI({
  apiKey: process.env.ANTHROPIC_API_KEY,
  baseURL: "http://localhost:3000/v1",
  defaultHeaders: { "x-provider": "anthropic" }
});

const response = await openai.chat.completions.create({
  model: "claude-3-5-sonnet-latest",
  messages: [{ role: "user", content: "Hello!" }]
});
```

## SDK Integration

### Node.js (using Anthropic SDK)
```typescript
import Anthropic from '@anthropic-ai/sdk';

const anthropic = new Anthropic({
  apiKey: process.env.ANTHROPIC_API_KEY,
  baseURL: "http://localhost:3000/v1",
  defaultHeaders: { "x-provider": "anthropic" }
});
```

### Python
```python
from anthropic import Anthropic

client = Anthropic(
    api_key="your-api-key",
    base_url="http://localhost:3000/v1",
    default_headers={"x-provider": "anthropic"}
)
```

## Error Handling

| Error Code | Description | Solution |
|------------|-------------|----------|
| 401        | Invalid API key | Check your Anthropic API key |
| 429        | Rate limit exceeded | Implement backoff strategy |
| 500        | Server error | Check server logs |

## Best Practices

1. **API Version Header**
   - The provider automatically sets the correct Anthropic API version
   - Current version: `2023-06-01`

2. **Authentication**
   - Use Bearer token format
   - API key is automatically converted to x-api-key format

3. **Content Types**
   - Always use `application/json`
   - UTF-8 encoding required

## Limitations

1. **Rate Limits**
   - No rate limiting is performed by the gateway
   - Anthropic's native rate limits apply directly
   - Refer to [Anthropic's rate limit documentation](https://docs.anthropic.com/claude/reference/rate-limits)

2. **Model Availability**
   - Not all Claude models may be available
   - Check Anthropic's documentation for current model list

Note: When using Anthropic as the provider, the gateway 
automatically:
- Routes requests to Anthropic's message API
- Converts the Authorization Bearer token to the required 
x-api-key format
- Adds the required anthropic-version header

## Additional Resources

- [Anthropic API Documentation](https://docs.anthropic.com/claude/reference)
- [Claude Model Overview](https://docs.anthropic.com/claude/docs/models-overview)
- [Rate Limits](https://docs.anthropic.com/claude/reference/rate-limits) 