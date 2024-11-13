# MagicAPI AI Gateway

The world's fastest AI Gateway proxy, written in Rust and optimized for maximum performance. This high-performance API gateway routes requests to various AI providers (OpenAI, Anthropic, GROQ, Fireworks, Together) with streaming support, making it perfect for developers who need reliable and blazing-fast AI API access.

[![Rust](https://github.com/MagicAPI/ai-gateway/actions/workflows/rust.yml/badge.svg)](https://github.com/MagicAPI/ai-gateway/actions/workflows/rust.yml)
[![Crates.io](https://img.shields.io/crates/v/magicapi-ai-gateway.svg)](https://crates.io/crates/magicapi-ai-gateway)

## Features

- 🚀 Blazing fast performance - built in Rust with zero-cost abstractions
- ⚡ Optimized for low latency and high throughput
- 🔄 Unified API interface for multiple AI providers (OpenAI, Anthropic, GROQ, Fireworks, Together)
- 📡 Real-time streaming support with minimal overhead
- 🔍 Built-in health checking
- 🛡️ Configurable CORS
- 🔀 Smart provider-specific request routing
- 📊 Efficient request/response proxying
- 💪 Production-ready and battle-tested

## Quick Start

### Installation

You can install MagicAPI Gateway using one of these methods:

#### Using Cargo Install

```bash
cargo install magicapi-ai-gateway
```

After installation, you can start the gateway by running:
```bash
magicapi-ai-gateway
```

#### Building from Source

1. Clone the repository:
```bash
git clone https://github.com/magicapi/ai-gateway
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

You can configure the gateway using environment variables:

```bash
# Basic configuration
export RUST_LOG=info

# Start the gateway
magicapi-ai-gateway

# Or with custom port
PORT=8080 magicapi-ai-gateway
```

## Usage

### Making Requests

To make requests through the gateway, use the `/v1/*` endpoint and specify the provider using the `x-provider` header.

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

Note: When using Anthropic as the provider, the gateway automatically:
- Routes requests to Anthropic's message API
- Converts the Authorization Bearer token to the required x-api-key format
- Adds the required anthropic-version header

## Configuration

The gateway can be configured using environment variables:

```bash
RUST_LOG=debug # Logging level (debug, info, warn, error)
```

## Performance

MagicAPI Developer AI Gateway is designed for maximum performance:

- **Zero-cost abstractions** using Rust's ownership model
- **Asynchronous I/O** with Tokio for optimal resource utilization
- **Connection pooling** via Reqwest for efficient HTTP connections
- **Memory-efficient** request/response proxying
- **Minimal overhead** in the request path
- **Optimized streaming** response handling

## Architecture

The gateway leverages the best-in-class Rust ecosystem:

- **Axum** - High-performance web framework
- **Tokio** - Industry-standard async runtime
- **Tower-HTTP** - Robust HTTP middleware
- **Reqwest** - Fast and reliable HTTP client
- **Tracing** - Zero-overhead logging and diagnostics

## Security Notes

- Always run behind a reverse proxy in production
- Configure CORS appropriately for your use case
- Use environment variables for sensitive configuration
- Consider adding rate limiting for production use

## Contributing

We welcome contributions! Please see our [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

### Development Setup

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

## Support

For support, please open an issue in the GitHub repository. Our community is active and happy to help!

## License

This project is dual-licensed under both the MIT License and the Apache License (Version 2.0). You may choose either license at your option. See the [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE) files for details.

## Acknowledgments

Special thanks to all our contributors and the Rust community for making this project possible.

## Docker Support

### Building and Running with Docker

1. Build the Docker image:
```bash
docker build -t magicapi1/magicapi-ai-gateway:latest .
```

2. Run the container:
```bash
docker run -p 3000:3000 \
  -e RUST_LOG=info \
  magicapi1/magicapi-ai-gateway:latest
```

### Using Pre-built Docker Image

```bash
docker pull magicapi1/magicapi-ai-gateway:latest
docker run -p 3000:3000 \
  -e RUST_LOG=info \
  magicapi1/magicapi-ai-gateway:latest
```

### Docker Compose

#### Option 1: Build from Source

Create a `docker-compose.yml` file:

```yaml
version: '3.8'
services:
  gateway:
    build: .
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
    image: magicapi1/magicapi-ai-gateway:latest
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

## Release Process for magicapi-ai-gateway

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
git checkout -b release/v0.1.7

# Stage and commit changes
git add Cargo.toml CHANGELOG.md
git commit -m "chore: release v0.1.7"

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
- The new version appears on [crates.io](https://crates.io/crates/magicapi-ai-gateway)
- Documentation is updated on [docs.rs](https://docs.rs/magicapi-ai-gateway)
- The GitHub release is visible (if using GitHub)
```

This process follows Rust community best practices for releasing crates. Remember to:
- Follow semantic versioning (MAJOR.MINOR.PATCH)
- Test thoroughly before releasing
- Document all significant changes
- Keep your repository and crates.io package in sync

Would you like me to explain any part of this process in more detail?