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
- AWS Bedrock
- GROQ
- Fireworks AI
- Together AI

## 🚀 Quick Start

1. **Clone the repository:**
```bash
git clone https://github.com/Noveum/ai-gateway.git
cd ai-gateway
```

2. **Install dependencies:**
```bash
npm install
```

3. **Configure environment variables:**
Create a `.dev.vars` file in the root directory:
```env
OPENAI_API_KEY=your_openai_api_key
ANTHROPIC_API_KEY=your_anthropic_key
AWS_ACCESS_KEY_ID=your_aws_key
AWS_SECRET_ACCESS_KEY=your_aws_secret
AWS_REGION=us-east-1
GROQ_API_KEY=your_groq_key
FIREWORKS_API_KEY=your_fireworks_key
TOGETHER_API_KEY=your_together_key
```

4. **Development:**
```bash
npm run dev
# Server starts at http://localhost:3000
```

5. **Deploy to Cloudflare Workers:**
```bash
npm run deploy
```

## 📚 Usage Examples

### OpenAI Integration

#### Using cURL
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

#### Using cURL
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

### Fireworks AI Integration

#### Using cURL
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: fireworks" \
  -H "Authorization: Bearer your-fireworks-api-key" \
  -d '{
    "model": "accounts/fireworks/models/llama-v3p2-11b-vision-instruct",
    "messages": [
      {
        "role": "user",
        "content": [
          {
            "type": "text",
            "text": "What's in this image?"
          },
          {
            "type": "image_url",
            "image_url": {
              "url": "https://example.com/image.jpg"
            }
          }
        ]
      }
    ],
    "temperature": 0.7,
    "max_tokens": 300,
    "stream": true
  }'
```

### Together AI Integration

#### Using cURL
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
    "stop": ["<|eot_id|>", "<|eom_id|>"],
    "stream": true
  }'
```

### AWS Bedrock Integration

#### Using cURL
```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: bedrock" \
  -H "x-aws-access-key-id: YOUR_ACCESS_KEY" \
  -H "x-aws-secret-access-key: YOUR_SECRET_KEY" \
  -H "x-aws-region: us-east-1" \
  -d '{
    "model": "anthropic.claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}],
    "temperature": 0.7,
    "max_tokens": 1000
  }'
```

## 🏗 Architecture

### Project Structure
```
src/
  ├── providers/          # AI provider implementations
  │   ├── openai.ts      # OpenAI provider
  │   ├── anthropic.ts   # Anthropic provider
  │   ├── bedrock.ts     # AWS Bedrock provider
  │   ├── groq.ts        # GROQ provider
  │   ├── fireworks.ts   # Fireworks AI provider
  │   └── together.ts    # Together AI provider
  ├── middleware/         # Middleware components
  │   ├── auth.ts        # Authentication middleware
  │   ├── validation.ts  # Request validation
  │   └── logging.ts     # Request/response logging
  ├── hooks/             # Transformation hooks
  ├── handlers/          # Route handlers
  ├── types/             # TypeScript definitions
  └── index.ts          # Application entry
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

### Custom Middleware

Create custom middleware in \`src/middleware/\`:
```typescript
export const customMiddleware = async (
  c: Context,
  next: Next
) => {
  // Pre-processing
  await next();
  // Post-processing
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
