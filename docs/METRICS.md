# Metrics Documentation

The AI Gateway collects detailed metrics for every request. These metrics help monitor performance, usage, and costs across different providers.

## Metrics Structure

```typescript
interface RequestMetrics {
  requestId: string;          // Unique identifier for the request
  timestamp: string;          // ISO timestamp when request was made
  method: string;            // HTTP method (usually POST)
  path: string;              // API endpoint path
  provider: string;          // AI provider name (openai, anthropic, etc.)
  model?: string;            // Model used for the request
  status?: number;           // HTTP status code
  success: boolean;          // Whether request was successful
  cached: boolean;           // Whether response was cached
  performance: {
    startTime: number;       // Request start timestamp
    endTime?: number;        // Request end timestamp
    ttfb?: number;          // Time to first byte (ms)
    totalLatency?: number;   // Total request duration (ms)
  };
  tokens?: {
    input: number;          // Input token count
    output: number;         // Output token count
    total: number;          // Total tokens used
    details?: Record<string, any>; // Provider-specific token details
  };
  cost?: {
    inputCost?: number;     // Cost for input tokens
    outputCost?: number;    // Cost for output tokens
    totalCost?: number;     // Total cost for request
  };
  metadata?: {
    estimated: boolean;     // Whether metrics are estimated
    totalChunks: number;    // Number of chunks for streaming
    streamComplete?: boolean; // Whether stream completed successfully
    [key: string]: any;     // Additional provider-specific metadata
  };
}
```

## Example Metrics by Provider

### OpenAI
```json
{
  "requestId": "abc123",
  "timestamp": "2024-12-18T12:00:00.000Z",
  "method": "POST",
  "path": "/v1/chat/completions",
  "provider": "openai",
  "performance": {
    "startTime": 1734524178000,
    "ttfb": 250,
    "endTime": 1734524178500,
    "totalLatency": 500
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 15,
    "streamComplete": true,
    "systemFingerprint": "fp_1234"
  },
  "model": "gpt-4",
  "status": 200,
  "tokens": {
    "input": 50,
    "output": 100,
    "total": 150,
    "details": {
      "cachedTokens": 0,
      "audioTokens": 0,
      "reasoningTokens": 0
    }
  },
  "cost": {
    "inputCost": 0.0015,
    "outputCost": 0.003,
    "totalCost": 0.0045
  }
}
```

### Anthropic
```json
{
  "requestId": "def456",
  "timestamp": "2024-12-18T12:10:00.000Z",
  "method": "POST",
  "path": "/v1/chat/completions",
  "provider": "anthropic",
  "performance": {
    "startTime": 1734524178000,
    "ttfb": 300,
    "endTime": 1734524178800,
    "totalLatency": 800
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 20,
    "streamComplete": true,
    "messageId": "msg_123"
  },
  "model": "claude-3-sonnet-20240229-v1:0",
  "status": 200,
  "tokens": {
    "input": 40,
    "output": 120,
    "total": 160
  }
}
```

### Together
```json
{
  "requestId": "1c3f70e1-7eac-4b96-a41d-30dfe799596d",
  "timestamp": "2024-12-18T12:16:18.773Z",
  "method": "POST",
  "path": "/v1/chat/completions",
  "provider": "together",
  "performance": {
    "startTime": 1734524178773,
    "ttfb": 555,
    "endTime": 1734524179734,
    "totalLatency": 961
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 26,
    "streamComplete": true,
    "seed": 9180670685818186000
  },
  "model": "meta-llama/Llama-2-7b-chat-hf",
  "status": 200,
  "tokens": {
    "input": 34,
    "output": 75,
    "total": 109
  }
}
```

### GROQ
```json
{
  "requestId": "61cf76a3-c844-459d-a288-b73a149b005f",
  "timestamp": "2024-12-18T13:07:23.025Z",
  "method": "POST",
  "path": "/v1/chat/completions",
  "provider": "groq",
  "performance": {
    "startTime": 1734527243025,
    "ttfb": 765,
    "endTime": 1734527243809,
    "totalLatency": 784
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 76,
    "streamComplete": true
  },
  "model": "llama-3.1-8b-instant",
  "status": 200,
  "tokens": {
    "input": 45,
    "output": 207,
    "total": 252
  },
  "cost": {
    "inputCost": 0.00000225,
    "outputCost": 0.00001656,
    "totalCost": 0.00001881
  }
}
```

### Fireworks
```json
{
  "requestId": "db486b29-63fb-451f-8441-e2521de528c6",
  "timestamp": "2024-12-18T13:07:34.496Z",
  "method": "POST",
  "path": "/v1/chat/completions", 
  "provider": "fireworks",
  "performance": {
    "startTime": 1734527254496,
    "ttfb": 802,
    "endTime": 1734527255774,
    "totalLatency": 1278
  },
  "success": true,
  "cached": false,
  "metadata": {
    "estimated": false,
    "totalChunks": 24,
    "streamComplete": true
  },
  "model": "accounts/fireworks/models/llama-v3p2-11b-vision-instruct",
  "status": 200,
  "tokens": {
    "input": 6434,
    "output": 69,
    "total": 99
  },
  "cost": {
    "inputCost": 0.0006434,
    "outputCost": 0.0000069,
    "totalCost": 0.0006503
  }
}
```

## Provider-Specific Metrics

Each provider may include additional metadata or token details specific to their service:

### OpenAI
- System fingerprint
- Cached tokens
- Audio tokens
- Reasoning tokens
- Prediction tokens

### Anthropic
- Message ID
- Stop reason
- Input/output token details

### Together
- Seed value
- Model-specific metadata
- Finish reason

### GROQ
- Queue time
- Prompt time
- Completion time
- System fingerprint
- Model-specific token details
- Stream metrics

### Fireworks
- Model-specific metadata
- Request ID
- Finish reason
- Token usage details
- Stream performance metrics

## Cost Tracking

When token costs are configured, the metrics include detailed cost breakdown. Cost estimates are based on public pricing as of December 18, 2024:

```json
"cost": {
  "inputCost": 0.0015,    // Cost for input tokens
  "outputCost": 0.003,    // Cost for output tokens  
  "totalCost": 0.0045     // Total request cost
}
```

Costs are configured per provider in the following files under `/src/metrics/costs/`:

### OpenAI
- Configuration: [`openai.ts`](https://github.com/Noveum/ai-gateway/blob/main/src/metrics/costs/openai.ts)
- Includes pricing for GPT-4, GPT-4 Turbo, GPT-3.5 Turbo models
- Costs are defined in dollars per million tokens
- Supports model name normalization for matching variants

### Anthropic
- Configuration: [`anthropic.ts`](https://github.com/Noveum/ai-gateway/blob/main/src/metrics/costs/anthropic.ts)
- Includes pricing for Claude 3 Opus, Sonnet, and Haiku models
- Handles model version variations (e.g., claude-3 vs claude-3.5)
- Supports normalized model name matching

### Together AI
- Configuration: [`together.ts`](https://github.com/Noveum/ai-gateway/blob/main/src/metrics/costs/together.ts)
- Dynamic pricing based on model size and type (Lite, Turbo, Reference)
- Supports Llama, Mixtral, Qwen, and other models
- Includes specialized pricing for vision and code models

### GROQ
- Configuration: [`groq.ts`](https://github.com/Noveum/ai-gateway/blob/main/src/metrics/costs/groq.ts)
- Includes pricing for Llama, Mixtral, and Gemma models
- Handles context window variations in model names
- Supports base model matching without context window suffix

### Fireworks
- Configuration: [`fireworks.ts`](https://github.com/Noveum/ai-gateway/blob/main/src/metrics/costs/fireworks.ts)
- Tiered pricing based on model size (Small, Medium, Large)
- Special pricing for MoE models (Mixtral)
- Dynamic cost calculation based on model architecture

Each provider's cost configuration includes:
- Model name normalization for flexible matching
- Per-token cost calculation (converted from dollars per million tokens)
- Support for both input and output token pricing
- Fallback pricing for unlisted models based on size/type

To add or update costs for a provider:
1. Locate the provider's cost file in `/src/metrics/costs/`
2. Update the `BASE_COSTS` configuration
3. Costs should be specified in dollars per million tokens
4. Use the `MILLION` constant for conversion: `cost / MILLION`

## Streaming Metrics

For streaming responses, metrics are collected throughout the stream:
- TTFB is set on first chunk
- Token counts are updated with each chunk
- Final metrics are logged when stream completes
- Chunk count tracks total response segments 