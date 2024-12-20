# Noveum AI Gateway

<div align="center">

[![Apache License 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://github.com/Noveum/ai-gateway/blob/main/LICENSE)
[![Docker Build](https://github.com/Noveum/ai-gateway/actions/workflows/docker.yml/badge.svg)](https://github.com/Noveum/ai-gateway/actions/workflows/docker.yml)
[![Docker Pulls](https://img.shields.io/docker/pulls/noveum/ai-gateway)](https://hub.docker.com/r/noveum/ai-gateway)
[![Docker Image Size](https://img.shields.io/docker/image-size/noveum/ai-gateway/latest)](https://hub.docker.com/r/noveum/ai-gateway)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](https://github.com/Noveum/ai-gateway/pulls)
[![Contributors](https://img.shields.io/github/contributors/Noveum/ai-gateway)](https://github.com/Noveum/ai-gateway/graphs/contributors)
[![GitHub Release](https://img.shields.io/github/v/release/Noveum/ai-gateway)](https://github.com/Noveum/ai-gateway/releases)

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
- 🔒 **OpenAI Compatible**: Drop-in replacement for OpenAI's API

## 🤖 Supported Providers

| Provider | Streaming | OpenAI Compatible |
|----------|-----------|------------------|
| OpenAI | ✅ | Native |
| Anthropic | ✅ | ✅ |
| GROQ | ✅ | ✅ |
| Fireworks | ✅ | ✅ |
| Together | ✅ | ✅ |

## 🚀 Quick Start

### Using Cloudflare Workers (Recommended)

```bash
# Install Wrangler CLI
npm install -g wrangler

# Clone and Setup
git clone https://github.com/Noveum/ai-gateway.git
cd ai-gateway
npm install

# Login to Cloudflare
wrangler login

# Development
npm run dev     # Server starts at http://localhost:3000

# Deploy
npm run deploy
```

### Using Docker (Alternative)

```bash
docker pull noveum/ai-gateway:latest
docker run -p 3000:3000 noveum/ai-gateway:latest
```

## 📚 Usage Examples

### OpenAI-Compatible Interface

The gateway provides a drop-in replacement for OpenAI's API. You can use your existing OpenAI client libraries by just changing the base URL:

```typescript
// TypeScript/JavaScript
import OpenAI from 'openai';

const openai = new OpenAI({
    baseURL: 'http://localhost:3000/v1',
    apiKey: 'your-provider-api-key',
    defaultHeaders: { 'x-provider': 'openai' }
});

const response = await openai.chat.completions.create({
    model: 'gpt-4',
    messages: [{ role: 'user', content: 'Hello!' }]
});
```

### Provider-Specific Examples

#### Anthropic (Claude)
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

#### GROQ
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

#### Streaming Example
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:3000/v1",
    api_key="your-provider-api-key",
    default_headers={"x-provider": "anthropic"}  # or any other provider
)

stream = client.chat.completions.create(
    model="claude-3-sonnet-20240229-v1:0",
    messages=[{"role": "user", "content": "Write a story"}],
    stream=True
)

for chunk in stream:
    if chunk.choices[0].delta.content is not None:
        print(chunk.choices[0].delta.content, end="")
```

### Example Response with Metrics

```json
{
  "id": "chatcmpl-abc123",
  "object": "chat.completion",
  "created": 1709312768,
  "model": "gpt-4",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "Hello! How can I help you today?"
    },
    "finish_reason": "stop"
  }],
  "usage": {
    "prompt_tokens": 10,
    "completion_tokens": 9,
    "total_tokens": 19
  },
  "system_fingerprint": "fp_1234",
  "metrics": {
    "latency_ms": 450,
    "tokens_per_second": 42.2,
    "cost": {
      "input_cost": 0.0003,
      "output_cost": 0.0006,
      "total_cost": 0.0009
    }
  }
}
```

## 📖 Documentation

- [Deployment Guide](docs/DEPLOYMENT.md)
- [Metrics & Monitoring](docs/METRICS.md)
- [Contributing Guidelines](CONTRIBUTING.md)

## 📄 Contributing Opportunities

We welcome contributions! Here are some tasks we're actively looking for help with:

### High Priority Tasks
1. **AWS Bedrock Integration**
   - Add support for AWS Bedrock models
   - Implement authentication and cost tracking
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

2. **Testing Framework**
   - Set up unit and integration tests
   - Add provider-specific test cases
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

3. **Performance Benchmarks**
   - Create benchmarking suite
   - Compare with other AI gateways
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

### Feature Requests
4. **Prometheus Integration**
   - Add metrics exporter
   - Create Grafana dashboards
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

5. **Response Caching**
   - Implement caching layer
   - Add cache invalidation
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

6. **Rate Limiting**
   - Add per-user rate limits
   - Implement token bucket algorithm
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

### Documentation
7. **Provider Guides**
   - Create setup guides for each provider
   - Add troubleshooting sections
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

8. **Deployment Examples**
   - Add Docker Compose examples
   - Create cloud deployment guides
   - [Get Started →](https://github.com/Noveum/ai-gateway/issues/new)

Want to contribute? 
1. Pick a task from above
2. Open an issue to discuss your approach
3. Submit a pull request

Need help? Join our [Discord](https://discord.gg/noveum) or check existing [issues](https://github.com/Noveum/ai-gateway/issues).

## 📄 Metrics & Monitoring

The gateway collects detailed metrics for every request, providing insights into:
- 📈 Real-time performance tracking
- 💰 Token usage and cost calculation
- 🔄 Streaming metrics support
- 📊 Provider-specific metadata
- ⏱️ Latency and TTFB monitoring
- 🔍 Detailed debugging information

For detailed metrics documentation, see [METRICS.md](docs/METRICS.md)

## 📄 License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

## 📊 Contact

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
