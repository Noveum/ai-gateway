# Noveum AI Gateway

<div align="center">

[![Apache License 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://github.com/Noveum/ai-gateway/blob/main/LICENSE)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](https://github.com/Noveum/ai-gateway/pulls)
[![Contributors](https://img.shields.io/github/contributors/Noveum/ai-gateway)](https://github.com/Noveum/ai-gateway/graphs/contributors)

</div>

A hyper-efficient, lightweight AI Gateway that provides a unified interface to access various AI model providers through a single endpoint. Built for edge deployment using Cloudflare Workers, it offers seamless integration with popular AI providers while maintaining high performance and low latency.

## 🌟 Features

- 🚀 **Edge-Optimized Performance**: Built on Cloudflare Workers for minimal latency
- 🔄 **Universal Interface**: Single endpoint for multiple AI providers
- 🔌 **Provider Agnostic**: Easily switch between different AI providers
- 📡 **Streaming Support**: Real-time streaming responses for all supported providers
- 🛠 **Extensible Middleware**: Customizable request/response pipeline
- ✅ **Built-in Validation**: Automatic request validation and error handling
- 🔄 **Auto-Transform**: Automatic request/response transformation
- 📝 **Detailed Metrics**: Comprehensive request metrics and cost tracking
- 📝 **Comprehensive Logging**: Detailed logging for monitoring and debugging
- 💪 **Type-Safe**: Built with TypeScript for robust type safety

## 🤖 Supported Providers

- OpenAI (GPT-4, GPT-3.5)
- Anthropic (Claude 3)
- GROQ
- Fireworks AI
- Together AI

## 🚀 Quick Start

### Option 1: Cloudflare Workers (Recommended)

1. **Install Wrangler CLI:**
```bash
npm install -g wrangler
```

2. **Clone and Setup:**
```bash
git clone https://github.com/Noveum/ai-gateway.git
cd ai-gateway
npm install
```

3. **Login to Cloudflare:**
```bash
wrangler login
```

4. **Configure Elasticsearch (Optional):**
```bash
wrangler secret put ELASTICSEARCH_HOST
wrangler secret put ELASTICSEARCH_PORT
wrangler secret put ELASTICSEARCH_PASSWORD
wrangler secret put ELASTICSEARCH_INDEX
```

5. **Development:**
```bash
npm run dev
# Server starts at http://localhost:3000
```

6. **Deploy:**
```bash
npm run deploy
```

Your AI Gateway will be deployed to Cloudflare's edge network, providing:
- Global distribution and low latency
- Auto-scaling and DDoS protection
- Zero cold starts
- Edge caching capabilities

For detailed deployment instructions, see [DEPLOYMENT.md](DEPLOYMENT.md)

### Option 2: Docker (Alternative)

If you can't use Cloudflare Workers, you can run the gateway using Docker:

```bash
docker pull noveum/ai-gateway:latest
docker run -p 3000:3000 noveum/ai-gateway:latest
```

For Docker deployment options and configuration, see [DEPLOYMENT.md](DEPLOYMENT.md)

## 📚 Usage Examples

### OpenAI Integration

API keys are passed in request headers for security. Each request should include the provider and its corresponding API key.

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer your-openai-api-key" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "stream": true
  }'
```

### Anthropic Integration

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "Authorization: Bearer your-anthropic-api-key" \
  -d '{
    "model": "claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "max_tokens": 1000
  }'
```

### GROQ Integration

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: groq" \
  -H "Authorization: Bearer your-groq-api-key" \
  -d '{
    "model": "mixtral-8x7b-32768",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "max_tokens": 1000
  }'
```

### Fireworks AI Integration

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: fireworks" \
  -H "Authorization: Bearer your-fireworks-api-key" \
  -d '{
    "model": "accounts/fireworks/models/llama-v3-7b",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "max_tokens": 300,
    "stream": true
  }'
```

### Together AI Integration

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -H "Authorization: Bearer your-together-api-key" \
  -d '{
    "model": "meta-llama/Llama-2-7b-chat-hf",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 512,
    "temperature": 0.7,
    "top_p": 0.7,
    "top_k": 50,
    "repetition_penalty": 1,
    "stream": true
  }'
```

Note: All provider API keys are passed securely in request headers, not environment variables. This allows for:
- Per-request API key flexibility
- Multiple API keys for the same provider
- Better security through request-scoped credentials
- No need to restart the server when changing API keys

## 🏗 Architecture

### Project Structure
```
/src/
├── handlers
│   └── chat.ts
├── hooks
│   └── index.ts
├── index.ts
├── metrics
│   ├── collector.ts
│   └── costs
│       ├── anthropic.ts
│       ├── fireworks.ts
│       ├── groq.ts
│       ├── index.ts
│       ├── openai.ts
│       ├── together.ts
│       └── types.ts
├── middleware
│   ├── auth.ts
│   ├── logging.ts
│   ├── types.ts
│   └── validation.ts
├── providers
│   ├── anthropic.ts
│   ├── base.ts
│   ├── factory.ts
│   ├── fireworks.ts
│   ├── groq.ts
│   ├── openai.ts
│   └── together.ts
├── types
│   └── index.ts
└── utils
```

### Key Components

#### Middleware System
- **Authentication**: Provider-specific API key validation
- **Validation**: Request payload validation
- **Logging**: Comprehensive request/response logging
- **CORS**: Cross-origin resource sharing
- **Error Handling**: Standardized error responses

#### Provider Interface
Each provider implements the \`AIProvider\` interface:
```typescript
interface AIProvider {
  chat(request: ChatRequest): Promise<ChatResponse>;
  stream(request: ChatRequest): ReadableStream;
  validate(request: ChatRequest): void;
}
```

## 🛠 Development Guide

### Adding a New Provider

1. Create a new provider class in \`src/providers/\`:
```typescript
import { AIProvider } from '../types';

export class NewProvider implements AIProvider {
  async chat(request: ChatRequest): Promise<ChatResponse> {
    // Implementation
  }

  stream(request: ChatRequest): ReadableStream {
    // Implementation
  }

  validate(request: ChatRequest): void {
    // Implementation
  }
}
```

2. Register the provider in \`src/providers/index.ts\`
3. Add provider-specific types in \`src/types/\`
4. Update the provider factory

### Adding Provider Metrics and Costs

1. Create a cost file in \`src/metrics/costs/\`:
```typescript
import { ProviderModelCosts } from './types';

// Convert price from dollars per million tokens to dollars per token
const MILLION = 1_000_000;

// Define pricing structure
const BASE_COSTS: ProviderModelCosts = {
  'model-name': {
    inputTokenCost: 0.50 / MILLION,  // $0.50 per million tokens
    outputTokenCost: 0.50 / MILLION
  }
};

// Add helper functions for model name normalization
const normalizeModelName = (model: string): string => {
  return model.toLowerCase()
    .replace(/[-_\s]/g, '')
    .replace(/\.(\d)/g, '$1')
    .replace(/v\d+(\.\d+)?/, '');
};
```

## 🤝 Contributing

We love your input! We want to make contributing to Noveum AI Gateway as easy and transparent as possible, whether it's:

- Reporting a bug
- Discussing the current state of the code
- Submitting a fix
- Proposing new features
- Becoming a maintainer

### Getting Started

1. Fork the repository
2. Create your feature branch:
   ```bash
   git checkout -b feature/amazing-feature
   ```
3. Commit your changes:
   ```bash
   git commit -m 'Add amazing feature'
   ```
4. Push to the branch:
   ```bash
   git push origin feature/amazing-feature
   ```
5. Open a Pull Request

### Development Process

1. Create an issue for any major changes and enhancements
2. Fork the repo and create your branch from `main`
3. Make your changes
4. Add tests for any new functionality
5. Ensure the test suite passes
6. Make sure your code lints
7. Issue that pull request!

For more detailed information about contributing, please see our [CONTRIBUTING.md](CONTRIBUTING.md) file.

### Code Style

- Use TypeScript for all new code
- Follow the existing code style
- Use meaningful variable names
- Add comments for complex logic
- Keep functions small and focused
- Write unit tests for new features

### Commit Messages

- Use the present tense ("Add feature" not "Added feature")
- Use the imperative mood ("Move cursor to..." not "Moves cursor to...")
- Limit the first line to 72 characters or less
- Reference issues and pull requests liberally after the first line

### Pull Request Process

1. Update the README.md with details of changes if needed
2. Update the CHANGELOG.md with details of changes
3. The PR will be merged once you have the sign-off of at least one maintainer

## 📄 License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

## 📊 Metrics & Monitoring

The gateway collects detailed metrics for every request, providing insights into performance, usage, and costs.

### Metrics Exporters

The gateway supports exporting metrics to various destinations. Currently supported exporters:

#### Elasticsearch Exporter

Configure the Elasticsearch exporter using environment variables:

```env
ELASTICSEARCH_HOST=your-elasticsearch-host
ELASTICSEARCH_PORT=9200  # Optional, defaults to 9200
ELASTICSEARCH_USERNAME=elastic  # Optional, defaults to 'elastic'
ELASTICSEARCH_PASSWORD=your-password
ELASTICSEARCH_INDEX=metrics  # Optional, defaults to 'metrics'
```

For detailed configuration and setup instructions, see [METRICS_EXPORTERS.md](docs/METRICS_EXPORTERS.md)

### Example Metrics
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
    "streamComplete": true
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

### Metrics Features
- 📈 Real-time performance tracking
- 💰 Token usage and cost calculation
- 🔄 Streaming metrics support
- 📊 Provider-specific metadata
- ⏱️ Latency and TTFB monitoring
- 🔍 Detailed debugging information

For detailed metrics documentation, see [METRICS.md](docs/METRICS.md)

## 🙏 Acknowledgments

- Built with [Hono](https://hono.dev/)
- Deployed on [Cloudflare Workers](https://workers.cloudflare.com/)

## 📬 Contact

- GitHub Issues: [https://github.com/Noveum/ai-gateway/issues](https://github.com/Noveum/ai-gateway/issues)
- Twitter: [@NoveumAI](https://twitter.com/NoveumAI)

---

<div align="center">
Made with ❤️ by the Noveum Team
</div>

```
Copyright 2024 Noveum AI

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

## 🙏 Acknowledgments

- Built with [Hono](https://hono.dev/)
- Deployed on [Cloudflare Workers](https://workers.cloudflare.com/)

## 📬 Contact

- GitHub Issues: [https://github.com/Noveum/ai-gateway/issues](https://github.com/Noveum/ai-gateway/issues)
- Twitter: [@NoveumAI](https://twitter.com/NoveumAI)

---

<div align="center">
Made with ❤️ by the Noveum Team
</div>
