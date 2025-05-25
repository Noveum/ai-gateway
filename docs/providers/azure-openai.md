# Azure OpenAI Provider

The Azure OpenAI provider allows routing requests to Azure-hosted OpenAI models through the Noveum AI Gateway. Azure OpenAI offers enterprise-grade OpenAI models with enhanced security, compliance, regional deployment options, and built-in content filtering.

## Features

- ✅ **Enterprise Security**: Uses Azure's enterprise-grade security and compliance
- ✅ **Content Filtering**: Built-in content filtering with detailed results in responses
- ✅ **Regional Deployment**: Deploy models in your preferred Azure regions
- ✅ **Streaming Support**: Full support for real-time streaming responses
- ✅ **Model Mapping**: Automatic deployment-to-model mapping for accurate metrics
- ✅ **Cost Tracking**: Automatic cost calculation based on Azure OpenAI pricing
- ✅ **Request ID Tracking**: Azure-specific request ID extraction for debugging

## Configuration

### Environment Variables

| Variable | Description | Default | Required |
|----------|-------------|---------|----------|
| `AZURE_OPENAI_RESOURCE_NAME` | Default Azure OpenAI resource name | (none) | No* |
| `AZURE_OPENAI_API_VERSION` | Default Azure OpenAI API version | `2025-01-01-preview` | No |

*Can be provided via `x-azure-resource-name` header instead

### Headers

| Header | Description | Required | Example |
|--------|-------------|----------|---------|
| `x-provider` | Must be set to `azure-openai` | ✅ Yes | `azure-openai` |
| `api-key` | Your Azure OpenAI API key | ✅ Yes | `0a18cca7bf284ec9988010d9392e8c68` |
| `x-azure-resource-name` | Azure OpenAI resource name | ✅ Yes* | `magicapi-openai` |
| `x-azure-api-version` | Azure OpenAI API version | No | `2025-01-01-preview` |

*Required unless set via `AZURE_OPENAI_RESOURCE_NAME` environment variable

## Usage

### Basic Chat Completion

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: azure-openai" \
  -H "api-key: your-azure-api-key" \
  -H "x-azure-resource-name: your-resource-name" \
  -d '{
    "model": "gpt-4.1",
    "messages": [
      {
        "role": "user", 
        "content": "Hello! Can you tell me a short joke?"
      }
    ],
    "max_tokens": 300
  }'
```

### Streaming Chat Completion

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: azure-openai" \
  -H "api-key: your-azure-api-key" \
  -H "x-azure-resource-name: your-resource-name" \
  -H "x-organization-id: your-org-id" \
  -H "x-project-id: your-project-id" \
  -d '{
    "model": "gpt-4.1",
    "messages": [
      {
        "role": "user", 
        "content": "Write a short story about a robot."
      }
    ],
    "max_tokens": 500,
    "stream": true
  }'
```

### Using Node.js SDK

```javascript
import OpenAI from 'openai';

const client = new OpenAI({
  baseURL: 'http://localhost:3000/v1',
  apiKey: 'not-used', // API key is provided via headers
  defaultHeaders: {
    'x-provider': 'azure-openai',
    'api-key': 'your-azure-api-key',
    'x-azure-resource-name': 'your-resource-name'
  }
});

const completion = await client.chat.completions.create({
  model: 'gpt-4.1',
  messages: [
    { role: 'user', content: 'Hello!' }
  ],
  stream: true
});

for await (const chunk of completion) {
  console.log(chunk.choices[0]?.delta?.content || '');
}
```

## Supported Models

The Azure OpenAI provider supports all models available in your Azure OpenAI deployment, including:

### Latest Models (2025)
- `gpt-4.1` - Latest GPT-4.1 model with enhanced capabilities
- `gpt-4.1-mini` - Faster, cost-effective GPT-4.1 variant
- `o3` - Advanced reasoning model
- `o3-mini` - Cost-effective reasoning model

### GPT-4o Series
- `gpt-4o` - Latest GPT-4 Omni model
- `gpt-4o-mini` - Cost-effective GPT-4o variant
- `gpt-4o-realtime-preview` - Real-time audio/video model

### GPT-4 Series
- `gpt-4-turbo` - GPT-4 Turbo model
- `gpt-4` - Standard GPT-4 model
- `gpt-4-32k` - GPT-4 with 32k context window

### GPT-3.5 Series
- `gpt-35-turbo` - GPT-3.5 Turbo (Azure naming)
- `gpt-35-turbo-16k` - GPT-3.5 Turbo with 16k context

### Other Models
- `text-embedding-3-small` - Small embedding model
- `text-embedding-3-large` - Large embedding model
- `whisper-1` - Speech-to-text model
- `dall-e-3` - Image generation model

## Response Format

Azure OpenAI responses include additional fields compared to standard OpenAI:

### Content Filtering Results

Azure OpenAI includes content filtering results in responses:

```json
{
  "choices": [
    {
      "content_filter_results": {
        "hate": {
          "filtered": false,
          "severity": "safe"
        },
        "self_harm": {
          "filtered": false,
          "severity": "safe"
        },
        "sexual": {
          "filtered": false,
          "severity": "safe"
        },
        "violence": {
          "filtered": false,
          "severity": "safe"
        }
      },
      "message": {
        "content": "Your response content here",
        "role": "assistant"
      }
    }
  ],
  "prompt_filter_results": [
    {
      "prompt_index": 0,
      "content_filter_results": {
        "hate": {
          "filtered": false,
          "severity": "safe"
        },
        "jailbreak": {
          "filtered": false,
          "detected": false
        },
        "self_harm": {
          "filtered": false,
          "severity": "safe"
        },
        "sexual": {
          "filtered": false,
          "severity": "safe"
        },
        "violence": {
          "filtered": false,
          "severity": "safe"
        }
      }
    }
  ]
}
```

## Error Handling

The Azure OpenAI provider handles various Azure-specific error conditions:

### Authentication Errors (401)
```json
{
  "error": {
    "message": "Invalid Azure OpenAI API key or insufficient permissions",
    "type": "authentication_error"
  }
}
```

### Resource Not Found (404)
```json
{
  "error": {
    "message": "The deployment 'your-deployment' does not exist",
    "type": "resource_not_found"
  }
}
```

### Rate Limiting (429)
```json
{
  "error": {
    "message": "Azure OpenAI rate limit exceeded",
    "type": "rate_limit_exceeded"
  }
}
```

### Content Filtering (400)
```json
{
  "error": {
    "message": "Content was filtered by Azure OpenAI",
    "type": "content_filter"
  }
}
```

## Advanced Features

### Model Parameter Transformation

The provider automatically transforms parameters for newer models:

- For `o1`, `o3`, `o4`, and `gpt-4.1` series models, `max_tokens` is automatically converted to `max_completion_tokens`
- This ensures compatibility with Azure's latest model requirements

### Request ID Tracking

Azure OpenAI responses include `x-request-id` headers that are automatically extracted for debugging and telemetry purposes.

### Cost Calculation

The provider automatically calculates costs based on Azure OpenAI pricing:

- Input tokens and output tokens are tracked separately
- Pricing varies by model (e.g., GPT-4.1: $0.012/1k input, $0.036/1k output)
- Costs are included in telemetry data when Elasticsearch integration is enabled

## Telemetry Integration

When [Elasticsearch integration](../elasticsearch-integration.md) is enabled, the Azure OpenAI provider automatically tracks:

- Request and response metrics
- Token usage and costs
- Content filtering results
- Azure-specific request IDs
- Model deployment mapping
- Provider latency

## Security Considerations

- **API Keys**: Azure API keys are never logged or exposed in responses
- **Content Filtering**: All requests go through Azure's content filtering system
- **Regional Compliance**: Deploy models in your preferred Azure regions for data residency
- **Enterprise Security**: Leverages Azure's enterprise-grade security and compliance features

## Troubleshooting

### Common Issues

1. **"Missing api-key header"**
   - Ensure you're using `api-key` header, not `Authorization: Bearer`
   - Azure OpenAI uses different authentication than standard OpenAI

2. **"Azure resource name is required"**
   - Provide `x-azure-resource-name` header or set `AZURE_OPENAI_RESOURCE_NAME` environment variable

3. **"Deployment not found"**
   - Verify the model name matches your Azure deployment name
   - Check your Azure OpenAI resource in the Azure portal

4. **Content filtering errors**
   - Review your request content for potentially harmful content
   - Azure OpenAI has stricter content filtering than standard OpenAI

### Debug Information

Enable debug logging to see detailed Azure OpenAI provider information:

```bash
RUST_LOG=debug noveum-ai-gateway
```

This will show:
- URL construction details
- Header processing
- Model mapping decisions
- Content filtering results
- Request ID tracking 