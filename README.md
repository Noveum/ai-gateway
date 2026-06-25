<div align="center">

# Noveum AI Gateway

🚀 The world's fastest AI Gateway proxy, written in Rust and optimized for maximum performance. This high-performance API gateway routes requests to various AI providers (OpenAI, Anthropic, GROQ, Fireworks, Together, AWS Bedrock) with streaming support, making it perfect for developers who need reliable and blazing-fast AI API access.

[![Rust](https://github.com/Noveum/ai-gateway/actions/workflows/rust.yml/badge.svg)](https://github.com/Noveum/ai-gateway/actions/workflows/rust.yml)
[![Crates.io](https://img.shields.io/crates/v/noveum-ai-gateway.svg)](https://crates.io/crates/noveum-ai-gateway)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE-MIT)
[![Docker Pulls](https://img.shields.io/docker/pulls/noveum/noveum-ai-gateway)](https://hub.docker.com/r/noveum/noveum-ai-gateway)

[Quick Start](#quick-start) • 
[Documentation](docs/) • 
[Pricing](docs/PRICING.md) • 
[Docker](docs/deployment.md) • 
[Contributing](docs/CONTRIBUTING.md)

</div>

## ✨ Features

- 🚀 **Blazing fast performance**: Built in Rust with zero-cost abstractions
- ⚡ **Optimized for low latency and high throughput**
- 🔄 **Unified API interface for multiple AI providers**:
  - OpenAI — [guide](docs/providers/openai.md)
  - AWS Bedrock — [guide](docs/providers/bedrock.md)
  - Anthropic — [guide](docs/providers/anthropic.md)
  - GROQ — [guide](docs/providers/groq.md)
  - Fireworks — [guide](docs/providers/fireworks.md)
  - Together AI — [guide](docs/providers/together.md)
  - Mistral, Cohere, Google Gemini, DeepSeek, xAI (Grok), OpenRouter, Perplexity — [OpenAI-compatible providers guide](docs/providers/openai-compatible.md)
- 💰 **Per-request cost tracking** across all providers from a built-in,
  single-sourced model pricing table — see [docs/PRICING.md](docs/PRICING.md).
- 📡 **Real-time Streaming**: Optimized for minimal latency
- 🛡️ **Nova Guard policy enforcement**: In-process guardrails — model allow/deny, regex, banned substrings, PII & secrets detection, JSON-schema validation, token caps — that block, redact, or flag requests and responses, loaded from a local policy bundle (file or inline). See [docs/NOVA_GUARD.md](docs/NOVA_GUARD.md).
- 🛡️ **Production Ready**: Battle-tested in high-load environments
- 🔍 **Health Checking**: Built-in monitoring
- 📊 **Telemetry & Metrics**: Per-request token usage and cost tracking with a pluggable `MetricsExporter` trait; ships with a console exporter (set `DEBUG_METRICS=true`) for local debugging. See [docs/telemetry-plugins.md](docs/telemetry-plugins.md).
- 🌐 **CORS Support**: Configurable cross-origin resource sharing
- 🛠️ **SDK Compatibility**: Works with any OpenAI-compatible SDK

## 🚀 Quick Start

### Installation

You can install Noveum Gateway using one of these methods:

### One Line Install & Run (With Cargo Install)

```bash
curl https://sh.rustup.rs -sSf | sh && cargo install noveum-ai-gateway && noveum-ai-gateway
```

#### Using Cargo Install

```bash
cargo install noveum-ai-gateway
```

After installation, you can start the gateway by running:
```bash
noveum-ai-gateway
```

#### Building from Source

1. Clone the repository:
```bash
git clone https://github.com/noveum/ai-gateway
cd ai-gateway
```

2. Build the project:
```bash
cargo build --release
```

3. Run the server:
```bash
cargo run --release
```

The server will start on `http://127.0.0.1:3000` by default.

### Running the Gateway

```bash
# Basic configuration
export RUST_LOG=info

# Start the gateway
noveum-ai-gateway

# Or with custom port
PORT=8080 noveum-ai-gateway
```

### Configuration (environment variables)

**Server / runtime**

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3000` | TCP port to listen on |
| `HOST` | `127.0.0.1` | Bind address |
| `WORKER_THREADS` | derived from CPU cores | Tokio worker thread count |
| `MAX_CONNECTIONS` | `10000` | Max idle HTTP connections kept per upstream host |
| `RUST_LOG` | `info` | Log filter (e.g. `info`, `debug`, `noveum_ai_gateway=debug`) |

**Telemetry (optional)**

| Variable | Default | Description |
|---|---|---|
| `DEBUG_METRICS` | `false` | Register the console metrics exporter (prints each request's token/cost metrics) |
| `DEPLOYMENT_ENVIRONMENT` | `development` | Value for the `deployment.environment` resource tag |

**Nova Guard policy enforcement (optional)** — see [docs/NOVA_GUARD.md](docs/NOVA_GUARD.md):

| Variable | Default | Description |
|---|---|---|
| `NOVEUM_GUARD_ENABLED` | `true` | Master switch for Nova Guard (accepts `false`/`0`/`no`/`off`/`disabled`) |
| `NOVEUM_GUARD_POLICIES_FILE` | — | Path to a local `nova-guard.json` policy bundle |
| `NOVEUM_GUARD_POLICIES` | — | Inline JSON policy bundle (alternative to the file) |
| `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` | `synthetic_success` | `synthetic_success` or `provider_error` |

## 📚 Usage Examples

### Making Requests

To make requests through the gateway, use the `/v1/*` endpoint and specify the provider using the `x-provider` header.

#### Example: AWS Bedrock Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: bedrock" \
  -H "x-aws-access-key-id: YOUR_ACCESS_KEY" \
  -H "x-aws-secret-access-key: YOUR_SECRET_KEY" \
  -H "x-aws-region: us-east-1" \
  -d '{
    "model": "anthropic.claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

#### Example: OpenAI Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer your-openai-api-key" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

#### Example: GROQ Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: groq" \
  -H "Authorization: Bearer your-groq-api-key" \
  -d '{
    "model": "llama2-70b-4096",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true,
    "max_tokens": 300
  }'
```

#### Example: Anthropic Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -H "Authorization: Bearer your-anthropic-api-key" \
  -d '{
    "model": "claude-3-5-sonnet-20241022",
    "messages": [{"role": "user", "content": "Write a poem"}],
    "stream": true,
    "max_tokens": 1024
  }'
```

#### Example: Fireworks Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: fireworks" \
  -H "Authorization: Bearer your-fireworks-api-key" \
  -d '{
    "model": "accounts/fireworks/models/llama-v3p1-8b-instruct",
    "messages": [{"role": "user", "content": "Write a poem"}],
    "stream": true,
    "max_tokens": 300,
    "temperature": 0.6,
    "top_p": 1,
    "top_k": 40
  }'
```

#### Example: Together AI Request

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: together" \
  -H "Authorization: Bearer your-together-api-key" \
  -d '{
    "model": "meta-llama/Llama-2-7b-chat-hf",
    "messages": [{"role": "user", "content": "Write a poem"}],
    "stream": true,
    "max_tokens": 512,
    "temperature": 0.7,
    "top_p": 0.7,
    "top_k": 50,
    "repetition_penalty": 1
  }'
```

## SDK Compatibility

The Noveum AI Gateway is designed to work seamlessly with popular AI SDKs. You can use the official OpenAI SDK to interact with any supported provider by simply configuring the baseURL and adding the appropriate provider header.

### Using with OpenAI's Official Node.js SDK

```typescript
import OpenAI from 'openai';

// Configure the SDK to use Noveum Gateway
const openai = new OpenAI({
  apiKey: process.env.PROVIDER_API_KEY, // Use any provider's API key
  baseURL: "http://localhost:3000/v1/", // Point to the gateway
  defaultHeaders: {
    "x-provider": "groq", // Specify the provider you want to use
  },
});

// Make requests as usual
const chatCompletion = await openai.chat.completions.create({
  messages: [
    { role: "system", content: "Write a poem" },
    { role: "user", content: "" }
  ],
  model: "llama-3.1-8b-instant",
  temperature: 1,
  max_tokens: 100,
  top_p: 1,
  stream: false,
});
```

You can easily switch between providers by changing the `x-provider` header and API key:

```typescript
// For OpenAI
const openaiClient = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "openai" },
});

// For AWS Bedrock
const bedrockClient = new OpenAI({
  apiKey: process.env.AWS_ACCESS_KEY_ID, // Use AWS access key
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: {
    "x-provider": "bedrock",
    "x-aws-access-key-id": process.env.AWS_ACCESS_KEY_ID,
    "x-aws-secret-access-key": process.env.AWS_SECRET_ACCESS_KEY,
    "x-aws-region": process.env.AWS_REGION || "us-east-1"
  },
});

// For Anthropic
const anthropicClient = new OpenAI({
  apiKey: process.env.ANTHROPIC_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "anthropic" },
});

// For GROQ
const groqClient = new OpenAI({
  apiKey: process.env.GROQ_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "groq" },
});

// For Fireworks
const fireworksClient = new OpenAI({
  apiKey: process.env.FIREWORKS_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "fireworks" },
});

// For Together AI
const togetherClient = new OpenAI({
  apiKey: process.env.TOGETHER_API_KEY,
  baseURL: "http://localhost:3000/v1/",
  defaultHeaders: { "x-provider": "together" },
});
```

The gateway automatically handles the necessary transformations to ensure compatibility with each provider's API format while maintaining the familiar OpenAI SDK interface.

### Testing Gateway URL
```
https://gateway.noveum.ai
```

### Send Example Request to Testing Gateway
```bash
curl --location 'https://gateway.noveum.ai/v1/chat/completions' \
  --header 'Authorization: Bearer YOUR_API_KEY' \
  --header 'Content-Type: application/json' \
  --header 'x-provider: groq' \
  --data '{
    "model": "llama-3.1-8b-instant",
    "messages": [
        {
            "role": "user",
            "content": "Write a poem"
        }
    ],
    "stream": true,
    "max_tokens": 300
}'
```

> **Note**: This deployment is provided for testing and evaluation purposes only. For production workloads, please deploy your own instance of the gateway or contact us for information about production-ready managed solutions.

## 🔧 Configuration

The gateway can be configured using environment variables:

```bash
RUST_LOG=debug # Logging level (debug, info, warn, error)
```

## 🏗️ Architecture

The gateway leverages the best-in-class Rust ecosystem:

- **Axum** - High-performance web framework
- **Tokio** - Industry-standard async runtime
- **Tower-HTTP** - Robust HTTP middleware
- **Reqwest** - Fast and reliable HTTP client
- **Tracing** - Zero-overhead logging and diagnostics

## 📈 Performance

Noveum Developer AI Gateway is designed for maximum performance:

- **Zero-cost abstractions** using Rust's ownership model
- **Asynchronous I/O** with Tokio for optimal resource utilization
- **Connection pooling** via Reqwest for efficient HTTP connections
- **Memory-efficient** request/response proxying
- **Minimal overhead** in the request path
- **Optimized streaming** response handling

## 🔒 Security Notes

- Always run behind a reverse proxy in production
- Configure CORS appropriately for your use case
- Use environment variables for sensitive configuration
- Consider adding rate limiting for production use

## 🤝 Contributing

We welcome contributions! Please see our [CONTRIBUTING.md](docs/CONTRIBUTING.md) for guidelines.

### 🛠️ Development Setup

```bash
# Install development dependencies
cargo install cargo-watch

# Run tests
cargo test

# Run with hot reload
cargo watch -x run
```

## Troubleshooting

### Common Issues

1. **Connection Refused**
   - Check if port 3000 is available
   - Verify the HOST and PORT settings

2. **Streaming Not Working**
   - Ensure `Accept: text/event-stream` header is set
   - Check client supports streaming
   - Verify provider supports streaming for the requested endpoint

3. **Provider Errors**
   - Verify provider API keys are correct
   - Check provider-specific headers are properly set
   - Ensure the provider endpoint exists and is correctly formatted

## 💬 Community

- [GitHub Discussions](https://github.com/noveum/ai-gateway/discussions)
- [Twitter](https://twitter.com/noveum-ai)

## 🙏 Acknowledgments

Special thanks to all [contributors](https://github.com/noveum/ai-gateway/graphs/contributors) and the Rust community.

## 📄 License

This project is dual-licensed under both the MIT License and the Apache License (Version 2.0). You may choose either license at your option. See the [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE) files for details.

## Docker Support

### Building and Running with Docker

1. Build the Docker image:
```bash
docker buildx build --platform linux/amd64 -t noveum/noveum-ai-gateway:latest . --load
```

2. Push the image to Docker Hub:
```bash
docker push noveum/noveum-ai-gateway:latest
```

3. Run the container:
```bash
docker run -p 3000:3000 \
  -e RUST_LOG=info \
  noveum/noveum-ai-gateway:latest
```

### Using Pre-built Docker Image

```bash
docker pull noveum/noveum-ai-gateway:latest
docker run -p 3000:3000 \
  -e RUST_LOG=info \
  noveum/noveum-ai-gateway:latest
```

### Using Pre-built Docker Image with Nova Guard policies

Mount a local policy bundle and point Nova Guard at it:

```bash
docker pull noveum/noveum-ai-gateway:latest  --platform linux/amd64
docker run --platform linux/amd64 -p 3000:3000 \
  -e RUST_LOG=info \
  -e NOVEUM_GUARD_POLICIES_FILE=/etc/nova-guard.json \
  -v "$(pwd)/nova-guard.json:/etc/nova-guard.json:ro" \
  noveum/noveum-ai-gateway:latest
```

### Docker Compose

For detailed deployment instructions, please refer to the [Deployment Guide](docs/deployment.md).

#### Option 1: Build from Source

Create a `docker-compose.yml` file:

```yaml
version: '3.8'
services:
  gateway:
    build: .
    platform: linux/amd64
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=info
    restart: unless-stopped
```

#### Option 2: Use Prebuilt Image

Create a `docker-compose.yml` file:

```yaml
version: '3.8'
services:
  gateway:
    image: noveum/noveum-ai-gateway:latest
    platform: linux/amd64
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=info
    restart: unless-stopped
```

Then run either option with:
```bash
docker-compose up -d
```

#### Option 3: Use Prebuilt Image with Nova Guard policies

Create a `docker-compose.yml` that mounts a local policy bundle:

```yaml
version: '3.8'
services:
  gateway:
    image: noveum/noveum-ai-gateway:latest
    platform: linux/amd64
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=info
      - NOVEUM_GUARD_POLICIES_FILE=/etc/nova-guard.json
    volumes:
      - ./nova-guard.json:/etc/nova-guard.json:ro
    restart: unless-stopped
```

Then run with:
```bash
docker-compose up -d
```

## Release Process for noveum-ai-gateway

### 1. Pre-release Checklist
- [ ] Update version number in `Cargo.toml`
- [ ] Update CHANGELOG.md (if you have one)
- [ ] Ensure all tests pass: `cargo test`
- [ ] Verify the crate builds locally: `cargo build --release`
- [ ] Run `cargo clippy` to check for any linting issues
- [ ] Run `cargo fmt` to ensure consistent formatting

### 2. Git Commands
```bash
# Create and switch to a release branch
git checkout -b release/v0.1.6

# Stage and commit changes
git add Cargo.toml CHANGELOG.md
git commit -m "chore: release v0.1.6"

# Create a git tag
git tag -a v0.1.7 -m "Release v0.1.7"

# Push changes and tag
git push origin release/v0.1.7
git push origin v0.1.7
```

### 3. Publishing to crates.io
```bash
# Verify the package contents
cargo package

# Publish to crates.io (requires authentication)
cargo publish
```

### 4. Post-release
1. Create a GitHub release (if using GitHub)
   - Go to Releases → Draft a new release
   - Choose the tag v0.1.7
   - Add release notes
   - Publish release

2. Merge the release branch back to main
```bash
git checkout main
git merge release/v0.1.7
git push origin main
```

### 5. Version Verification
After publishing, verify:
- The new version appears on [crates.io](https://crates.io/crates/noveum-ai-gateway)
- Documentation is updated on [docs.rs](https://docs.rs/noveum-ai-gateway)
- The GitHub release is visible (if using GitHub)

## Testing Deployment

Noveum provides a testing deployment of the AI Gateway, hosted in our London data centre. This deployment is intended for testing and evaluation purposes only, and should not be used for production workloads.

## 📊 OpenTelemetry Logging

Noveum AI Gateway now supports OpenTelemetry compatible logs for enhanced observability. The Gateway can export detailed request logs with a rich structured format that includes complete request/response details and performance metrics.

### Tracking Headers

You can add custom tracking information to your requests that will be included in the logs:

```bash
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -H "Authorization: Bearer your-openai-api-key" \
  -H "x-project-id: your-project-id" \
  -H "x-organisation-id: your-org-id" \
  -H "x-user-id: your-user-id" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

These headers will be included in the telemetry logs, allowing you to:

- Track usage by project
- Monitor costs per organization
- Analyze performance by user
- Segment analytics by experiment

For more details, see the [Telemetry Exporters Guide](docs/telemetry-plugins.md).

## Testing

Noveum Gateway includes comprehensive integration tests for all supported providers (OpenAI, Anthropic, GROQ, Fireworks, Together AI, and AWS Bedrock). These tests validate both non-streaming and streaming functionality.

### Running Integration Tests

1. Set up your test environment:
   ```bash
   # Copy the sample test environment file
   cp tests/.env.test.example .env.test
   
   # Edit the file to add your API keys for the providers you want to test
   nano .env.test
   ```

2. Start the gateway:
   ```bash
   cargo run
   ```

3. Run the integration tests:
   ```bash
   # Run all tests
   cargo test --test run_integration_tests -- --nocapture
   
   # Run tests for specific providers
   cargo test --test run_integration_tests openai -- --nocapture
   cargo test --test run_integration_tests anthropic -- --nocapture
   cargo test --test run_integration_tests groq -- --nocapture
   cargo test --test run_integration_tests fireworks -- --nocapture
   cargo test --test run_integration_tests together -- --nocapture
   cargo test --test run_integration_tests bedrock -- --nocapture
   ```

### Test Environment Configuration

Your `.env.test` file should include the following variables:

```bash
# Gateway URL (default: http://localhost:3000)
GATEWAY_URL=http://localhost:3000

# Provider API Keys - Add keys for the providers you want to test
OPENAI_API_KEY=your_openai_api_key
ANTHROPIC_API_KEY=your_anthropic_api_key
GROQ_API_KEY=your_groq_api_key
FIREWORKS_API_KEY=your_fireworks_api_key
TOGETHER_API_KEY=your_together_api_key

# AWS Bedrock Credentials
AWS_ACCESS_KEY_ID=your_aws_access_key_id
AWS_SECRET_ACCESS_KEY=your_aws_secret_access_key
AWS_REGION=us-east-1
```

For detailed test documentation, please refer to the [Integration Tests README](tests/README.md).
