# Noveum.ai Gateway

A hyper efficient, lightweight AI Gateway that allows you to use one endpoint to access various AI model providers. Built for edge deployment using Cloudflare Workers.

## Features

- Single endpoint for multiple AI providers
- Edge-optimized performance
- Provider-agnostic interface
- Streaming support
- Extensible middleware system
- Built-in validation and error handling
- Automatic request/response transformation
- Comprehensive logging
- Type-safe implementation

## Supported Providers

- OpenAI
- Anthropic
- AWS Bedrock
- GROQ (Coming Soon)
- Fireworks (Coming Soon)
- Together AI (Coming Soon)

## Quick Start

1. Clone the repository:
\`\`\`bash
git clone https://github.com/yourusername/noveum-ai-gateway.git
cd noveum-ai-gateway
\`\`\`

2. Install dependencies:
\`\`\`bash
npm install
\`\`\`

3. Configure environment variables:
Create a .dev.vars file in the root directory with your API keys:
\`\`\`
OPENAI_API_KEY=your_openai_key
ANTHROPIC_API_KEY=your_anthropic_key
AWS_ACCESS_KEY_ID=your_aws_key
AWS_SECRET_ACCESS_KEY=your_aws_secret
AWS_REGION=us-east-1
\`\`\`

4. Run locally:
\`\`\`bash
npm run dev
# Server will start at http://localhost:3000
\`\`\`

5. Deploy to Cloudflare Workers:
\`\`\`bash
npm run deploy
\`\`\`

## Usage

Make requests through the gateway using the /v1/* endpoint and specify the provider using the x-provider header.

### OpenAI Example

\`\`\`bash
curl -X POST http://localhost:3000/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "x-provider: openai" \\
  -H "Authorization: Bearer your-openai-api-key" \\
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
\`\`\`

### Anthropic Example

\`\`\`bash
curl -X POST http://localhost:3000/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "x-provider: anthropic" \\
  -H "Authorization: Bearer your-anthropic-api-key" \\
  -d '{
    "model": "claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
\`\`\`

### AWS Bedrock Example

\`\`\`bash
curl -X POST http://localhost:3000/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "x-provider: bedrock" \\
  -H "x-aws-access-key-id: YOUR_ACCESS_KEY" \\
  -H "x-aws-secret-access-key: YOUR_SECRET_KEY" \\
  -H "x-aws-region: us-east-1" \\
  -d '{
    "model": "anthropic.claude-3-sonnet-20240229-v1:0",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
\`\`\`

## Development

### Project Structure

\`\`\`
src/
  ├── providers/     # AI provider implementations
  ├── middleware/    # Middleware components
  ├── hooks/         # Request/response transformation hooks
  ├── handlers/      # Route handlers
  ├── types/         # TypeScript type definitions
  └── index.ts      # Main application entry
\`\`\`

### Adding a New Provider

1. Create a new provider class in src/providers/
2. Implement the AIProvider interface
3. Add the provider to the ProviderFactory
4. Update the Provider type in src/types/

### Middleware

The gateway includes several middleware components:

- Authentication
- Validation
- Logging
- CORS
- Pretty JSON

### Hooks

The hooks system allows you to transform requests and responses:

- beforeRequest: Modify the request before it reaches the provider
- afterResponse: Transform the provider's response
- onError: Custom error handling

## Contributing

1. Fork the repository
2. Create your feature branch
3. Commit your changes
4. Push to the branch
5. Create a Pull Request

## License

MIT
```

```
npm run deploy
```
