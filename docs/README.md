# Noveum AI Gateway documentation

Start with the page that matches what you are trying to do.

## Use the gateway

- [Five-minute quick start](../README.md#five-minute-quick-start)
- [Configuration and credential roles](configuration.md)
- [Nova Guard policies and tenancy](NOVA_GUARD.md)
- [Pricing and cost accounting](PRICING.md)
- [Telemetry and sensitive-data handling](logs.md)

## Upgrade, deploy, and operate

- [Migrate from v1.2 to v2](MIGRATING_TO_V2.md)
- [Native, Docker, and Kubernetes deployment](deployment.md)
- [Cloudflare Worker runbook](CLOUDFLARE_WORKER.md)
- [Cloudflare runtime architecture](CLOUDFLARE_DEPLOYMENT.md)
- [Validation checklist](VALIDATION.md)
- [Release procedure](RELEASING.md)

## Providers

- [OpenAI](providers/openai.md)
- [Anthropic](providers/anthropic.md)
- [Groq](providers/groq.md)
- [Fireworks](providers/fireworks.md)
- [Together AI](providers/together.md)
- [AWS Bedrock](providers/bedrock.md)
- [Mistral, Cohere, Gemini, DeepSeek, xAI, OpenRouter, and Perplexity](providers/openai-compatible.md)

Provider model availability changes independently of gateway releases. The
provider's own model catalog is the source of truth for whether a model can be
called; [`pricing/catalog.json`](../pricing/catalog.json) is the source of truth
for what this gateway version can estimate.

## Develop and contribute

- [Contributing](CONTRIBUTING.md)
- [Telemetry exporter interface](telemetry-plugins.md)
- [Current roadmap](TODO.md)
- [Changelog](../CHANGELOG.md)
- [Rust API documentation](https://docs.rs/noveum-ai-gateway)

[The v2.0.0 PR 30 validation record](PR30_NOVAGUARD_REVIEW.md) is archived, not
a current runbook. Use [Validation](VALIDATION.md) for repeatable checks.
