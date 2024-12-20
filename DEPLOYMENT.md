# Deployment Guide

## Preferred Method: Cloudflare Workers

### Prerequisites
- Cloudflare account
- Wrangler CLI installed (`npm install -g wrangler`)
- Node.js >= 18.0.0

### Deployment Steps

1. Login to Cloudflare:
```bash
wrangler login
```

2. Configure environment variables in Cloudflare (Optional):
```bash
# Set Elasticsearch configuration if you want to collect metrics
wrangler secret put ELASTICSEARCH_HOST
wrangler secret put ELASTICSEARCH_PORT
wrangler secret put ELASTICSEARCH_PASSWORD
wrangler secret put ELASTICSEARCH_INDEX
```

3. Deploy to Cloudflare Workers:
```bash
npm run deploy
```

Your AI Gateway will be deployed to Cloudflare's edge network, providing:
- Global distribution and low latency
- Auto-scaling and DDoS protection
- Zero cold starts
- Edge caching capabilities

## Alternative: Docker Deployment

If you can't use Cloudflare Workers, you can use Docker as an alternative deployment method.

### Using Pre-built Image

Basic run:
```bash
docker pull noveum/ai-gateway:latest
docker run -p 3000:3000 noveum/ai-gateway:latest
```

Full example with Elasticsearch configuration:
```bash
docker run -d \
  --name ai-gateway \
  -p 3000:3000 \
  -e ELASTICSEARCH_HOST="https://es.magicapi.dev" \
  -e ELASTICSEARCH_PORT="443" \
  -e ELASTICSEARCH_PASSWORD="your_password" \
  -e ELASTICSEARCH_INDEX="noveum_logs" \
  -e ELASTICSEARCH_USERNAME="elastic" \
  --restart unless-stopped \
  noveum/ai-gateway:latest
```

Available environment variables:
| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| ELASTICSEARCH_HOST | No | - | Elasticsearch host URL (with protocol) |
| ELASTICSEARCH_PORT | No | 9200 | Elasticsearch port |
| ELASTICSEARCH_USERNAME | No | elastic | Username for authentication |
| ELASTICSEARCH_PASSWORD | No | - | Password for authentication |
| ELASTICSEARCH_INDEX | No | noveum_logs | Index name for storing metrics |

Note: All environment variables are optional. If Elasticsearch is not configured, the gateway will run without metrics collection.

### Building Docker Image

1. Build the image using the provided script:
```bash
npm run docker:build
```

This will:
- Build an optimized multi-stage Docker image
- Create a minimal production image with only necessary files
- Tag it as both latest and version-specific
- Show commands to push to Docker Hub

The Docker image is optimized for production:
- Uses multi-stage builds to minimize image size
- Only includes production dependencies
- Runs as non-root user for security
- Based on Alpine Linux for minimal footprint

2. Push to Docker Hub (if you have access):
```bash
docker push noveum/ai-gateway:<version>
docker push noveum/ai-gateway:latest
```

### Environment Variables

Only infrastructure-related variables are configured through environment variables. API keys for AI providers (OpenAI, Anthropic, etc.) must be passed in request headers.

Create a `.env` file:
```env
# Elasticsearch Configuration (Optional)
ELASTICSEARCH_HOST=your_elasticsearch_host
ELASTICSEARCH_PORT=9200
ELASTICSEARCH_PASSWORD=your_password
ELASTICSEARCH_INDEX=noveum_logs
```

### Docker Compose

```yaml
version: '3.8'
services:
  ai-gateway:
    image: noveum/ai-gateway:latest
    ports:
      - "3000:3000"
    environment:
      # Only infrastructure-related environment variables
      - ELASTICSEARCH_HOST=your_elasticsearch_host
      - ELASTICSEARCH_PORT=9200
      - ELASTICSEARCH_PASSWORD=your_password
      - ELASTICSEARCH_INDEX=noveum_logs
    restart: unless-stopped