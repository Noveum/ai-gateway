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

### Fireworks
- Model-specific metadata
- Request ID
- Finish reason

## Cost Tracking

When token costs are configured, the metrics include detailed cost breakdown:
```json
"cost": {
  "inputCost": 0.0015,    // Cost for input tokens
  "outputCost": 0.003,    // Cost for output tokens
  "totalCost": 0.0045     // Total request cost
}
```

Configure costs per provider in your environment:
```env
OPENAI_INPUT_TOKEN_COST=0.00003
OPENAI_OUTPUT_TOKEN_COST=0.00006
ANTHROPIC_INPUT_TOKEN_COST=0.00001
ANTHROPIC_OUTPUT_TOKEN_COST=0.00003
```

## Streaming Metrics

For streaming responses, metrics are collected throughout the stream:
- TTFB is set on first chunk
- Token counts are updated with each chunk
- Final metrics are logged when stream completes
- Chunk count tracks total response segments 