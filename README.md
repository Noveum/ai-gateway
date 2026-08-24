<div align="center">

# Noveum AI Gateway

An OpenAI-compatible, multi-provider AI gateway written in Rust, with streaming,
provider-aware cost accounting, and Nova Guard policy enforcement.

[![Rust](https://github.com/Noveum/ai-gateway/actions/workflows/rust.yml/badge.svg)](https://github.com/Noveum/ai-gateway/actions/workflows/rust.yml)
[![Crates.io](https://img.shields.io/crates/v/noveum-ai-gateway.svg)](https://crates.io/crates/noveum-ai-gateway)
[![docs.rs](https://docs.rs/noveum-ai-gateway/badge.svg)](https://docs.rs/noveum-ai-gateway)
[![License: MIT/Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](https://github.com/Noveum/ai-gateway/blob/v2.0.1/LICENSE-MIT)

[Quick start](#five-minute-quick-start) ·
[Documentation](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/README.md) ·
[v2 migration](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/MIGRATING_TO_V2.md) ·
[Nova Guard](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/NOVA_GUARD.md) ·
[Operations](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/deployment.md)

</div>

## What it does

The gateway exposes one OpenAI Chat Completions surface. Select an upstream with
the `x-provider` header; an absent header defaults to OpenAI. It supports:

- OpenAI, Anthropic, Groq, Fireworks, Together AI, and AWS Bedrock;
- OpenAI-compatible routes for Mistral, Cohere, Gemini, DeepSeek, xAI,
  OpenRouter, and Perplexity;
- buffered and SSE responses, including Anthropic Messages-to-OpenAI stream
  translation and Bedrock Converse translation (Bedrock streaming is native
  only in v2.0.1);
- a versioned pricing catalog with token, cache, long-context, and supported
  tool-fee accounting;
- local or platform-managed Nova Guard policies; and
- native binary, Docker, Rust library, and Cloudflare Worker deployment shapes.

Provider features are not identical. Check the relevant
[provider guide](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/README.md#providers)
before relying on tools, vision,
provider-native fields, or strict cost-cap admission.

## Version 2.0.1

The 2.0 line introduced platform-managed Nova Guard and its SemVer-major public
API changes. Version 2.0.1 adds the static landing page, clearer runtime
boundaries, and post-release hardening described in the
[changelog](https://github.com/Noveum/ai-gateway/blob/v2.0.1/CHANGELOG.md#201---2026-08-24).
Release resources:

- [crates.io package](https://crates.io/crates/noveum-ai-gateway)
- [Rust API documentation](https://docs.rs/noveum-ai-gateway)
- [GitHub releases](https://github.com/Noveum/ai-gateway/releases)

Source, package, documentation, container, and production deployment can advance
at different times. Confirm that each surface reports 2.0.1 before treating it
as the release artifact or promotion target.

The 2.0 line is SemVer-major relative to 1.x. Library consumers must account
for changes to
`AppState::new`, `Policy::policy_type`, and `PolicyEngine::from_env`; follow the
[v1.2 → v2 migration guide](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/MIGRATING_TO_V2.md).

## Five-minute quick start

### 1. Install and start

Rust 1.94.1 or newer is required. Choose the registry package after the release
is published, or build a reviewed source checkout directly.

```bash
# Registry installation (available after v2.0.1 is published)
cargo install noveum-ai-gateway --version 2.0.1 --locked
RUST_LOG=info noveum-ai-gateway
```

```bash
# Reviewed source checkout (also works before registry publication)
cargo build --release --locked
RUST_LOG=info ./target/release/noveum-ai-gateway
```

The native server listens on `http://127.0.0.1:3000` by default and honors
`HOST`. Confirm the exact binary version before sending provider traffic:

```bash
curl --fail --silent http://127.0.0.1:3000/health
# {"status":"healthy","version":"2.0.1"}
```

### 2. Send one provider request

Provider credentials are supplied per request; the gateway does not keep them
in its configuration. This OpenAI example uses an explicit output bound, which
also makes it compatible with strict Nova Guard cost caps:

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: openai" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Reply with: gateway ready"}],
    "max_tokens": 32
  }'
```

Change `x-provider`, the provider credential, and the model to switch
upstreams. See the [provider examples](#provider-authentication-examples).

### Build from source instead

```bash
git clone https://github.com/Noveum/ai-gateway.git
cd ai-gateway
cargo run --locked --release
```

Run `cargo test --locked --lib` before modifying the source. The full release
matrix is in
[Validation](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/VALIDATION.md).

## HTTP surface

| Route | Purpose |
|---|---|
| `GET /`, `HEAD /` | Beginning with v2.0.1, a static no-JavaScript information page showing runtime/version and links to health, docs.rs, GitHub, and Noveum. `HEAD` returns the same status and security/cache headers without a body. |
| `GET /health` | Process/runtime health and exact package version. It does not test provider credentials. |
| `/v1/*` | Provider proxy surface; `POST /v1/chat/completions` is the documented contract. |

The landing page is not an admin console or hosted API explorer.
`/docs` and `/openapi.json` remain 404; use
[docs.rs](https://docs.rs/noveum-ai-gateway) and this documentation instead.

## Authentication: three different credentials

These credentials have different jobs and must not be substituted for one
another:

| Credential | Where it goes | Purpose |
|---|---|---|
| Provider API key | Usually `Authorization: Bearer …` on each request; Anthropic also accepts `x-api-key`; Bedrock uses `x-aws-*` headers | Authenticates the upstream model call and is forwarded only to that provider |
| Dedicated Noveum service key | `NOVEUM_API_KEY` deployment secret | Lets one dedicated gateway fetch policies/state and admit/settle usage for the fixed `NOVEUM_GUARD_PROJECT_ID` |
| Shared-mode caller key | `x-noveum-api-key` on each request to a **native** shared gateway | Authenticates the caller to Noveum and derives the caller's entitled project/organization; it is stripped before provider dispatch |

`x-project-id` and `x-organization-id` are not credentials. In transparent or
dedicated mode they are attribution headers. In shared mode they may only
select or confirm an identity already derived from the caller's Noveum key.

The transparent and dedicated gateway modes do **not** provide general-purpose
client authentication. Put a production deployment behind an authenticated
load balancer, API gateway, Cloudflare Access, or another access-control layer
when the endpoint must not be public.

### Noveum key scopes

Use the least-privileged key that fits the tenancy mode:

| Scope | Dedicated service key | Shared caller key |
|---|:---:|:---:|
| `guardrails:read` | required | required |
| `guardrails:ingest` | required | required |
| `projects:read` | not required | required |

Never put `NOVEUM_API_KEY` in `wrangler.toml`, a Docker image, source control,
or a request header. Store it in the runtime's secret manager.

## Provider authentication examples

All examples use the OpenAI Chat Completions request shape.

### Anthropic

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $ANTHROPIC_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: anthropic" \
  -d '{
    "model": "claude-haiku-4-5-20251001",
    "messages": [{"role": "user", "content": "Reply with one sentence."}],
    "max_tokens": 64
  }'
```

The gateway converts the request to Anthropic Messages and converts successful
buffered/SSE responses back to OpenAI shape. See the
[Anthropic compatibility contract](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/anthropic.md).

### Groq, Fireworks, Together, and other compatible providers

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer $GROQ_API_KEY" \
  -H "Content-Type: application/json" \
  -H "x-provider: groq" \
  -d '{
    "model": "openai/gpt-oss-20b",
    "messages": [{"role": "user", "content": "Reply with one sentence."}],
    "max_tokens": 64,
    "stream": true,
    "stream_options": {"include_usage": true}
  }'
```

Use the upstream provider's current model list; model availability is not
frozen by the gateway. See
[Groq](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/groq.md),
[Fireworks](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/fireworks.md),
[Together](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/together.md),
and
[other OpenAI-compatible providers](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/openai-compatible.md).

### AWS Bedrock

```bash
curl --fail-with-body http://127.0.0.1:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-provider: bedrock" \
  -H "x-aws-access-key-id: $AWS_ACCESS_KEY_ID" \
  -H "x-aws-secret-access-key: $AWS_SECRET_ACCESS_KEY" \
  -H "x-aws-region: ${AWS_REGION:-us-east-1}" \
  -d '{
    "model": "amazon.nova-micro-v1:0",
    "messages": [{"role": "user", "content": "Reply with one sentence."}],
    "max_tokens": 64
  }'
```

Both native and Cloudflare runtimes accept `x-aws-session-token` for temporary
STS credentials. Use least-privileged, short-lived AWS credentials and only
send credential headers over TLS to a gateway you control. Do not send AWS
credentials through the public/shared `gate.noveum.ai` endpoint. See the
[Bedrock guide](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/providers/bedrock.md).

When the environment contains temporary credentials, add this header to the
Bedrock request; omit it for a long-lived access-key pair:

```bash
-H "x-aws-session-token: $AWS_SESSION_TOKEN"
```

## OpenAI SDK example

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.GROQ_API_KEY,
  baseURL: "http://127.0.0.1:3000/v1",
  defaultHeaders: { "x-provider": "groq" },
});

const completion = await client.chat.completions.create({
  model: "openai/gpt-oss-20b",
  messages: [{ role: "user", content: "Reply with one sentence." }],
  max_tokens: 64,
});

console.log(completion.choices[0]?.message.content);
```

In native shared-tenancy mode, also add `x-noveum-api-key` and, if the key can
access several projects, `x-project-id` to `defaultHeaders`.

## Nova Guard deployment choices

| Mode | Runtime | Tenant identity | Appropriate use |
|---|---|---|---|
| Transparent | Native or Worker | none | Routing only; no platform policy/state bridge |
| Local stateless policies | Native or Worker | process-wide bundle | Regex, PII, secrets, allowlists, schema, and token policies without the Noveum control plane |
| Platform-managed dedicated | Native or Worker | fixed deployment key + project | One team's gateway or one Worker/domain per project |
| Platform-managed shared | **Native only** | derived from each caller's Noveum key | A hostname serving several projects or organizations |

Do not enable a dedicated project binding on a shared public hostname: every
call would be attributed and capped against that one project. A shared-hostname
Cloudflare Worker must remain transparent, be split into dedicated Workers per
project, or be replaced by the native shared-tenancy gateway.

The production Cloudflare endpoint `https://gate.noveum.ai` runs in
**transparent** mode for this reason. Check its public `/health` response for
the exact deployed version; v2.0.0 was the verified 2026-08-24 release baseline.
Provider requests still require the caller's provider credential. This status
is operational information, not a guarantee that the public endpoint is an
SLA-backed managed service.

Read
[Nova Guard](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/NOVA_GUARD.md)
for policy semantics and
[Cloudflare Worker operations](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/CLOUDFLARE_WORKER.md)
before enabling the platform bridge.

## Configuration

The common native settings are:

| Variable | Default | Purpose |
|---|---|---|
| `HOST` | `127.0.0.1` | Listen address. Use `0.0.0.0` inside a container. |
| `PORT` | `3000` | Listen port |
| `RUST_LOG` | `info` | Tracing filter |
| `DEBUG_METRICS` | `false` | Emit request metrics to the console; may include prompt/response content |
| `DEPLOYMENT_ENVIRONMENT` | `development` | Telemetry resource label |
| `NOVEUM_GUARD_ENABLED` | `true` | Nova Guard master switch; no source means transparent pass-through |

The complete, mode-aware reference is in
[Configuration](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/configuration.md).
Invalid or half-applied guard
configuration is rejected instead of silently disabling enforcement: at native
startup, or on Worker proxy requests while its information/health routes remain
available.

## Deploy and operate

- [Native, Docker, and Kubernetes](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/deployment.md)
- [Cloudflare Worker runbook](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/CLOUDFLARE_WORKER.md)
- [Cloudflare architecture and runtime differences](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/CLOUDFLARE_DEPLOYMENT.md)
- [Validation checklist](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/VALIDATION.md)
- [Telemetry and log handling](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/logs.md)
- [Release process](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/RELEASING.md)

Always pin an immutable version in production. Confirm `/health`, run at least
one buffered and one streaming request with a low output limit, inspect errors
and reservation settlement, and record the previous deployment identifier
before promotion.

Container automation publishes `2.0.1` and `latest` to both GHCR and Docker Hub
only when the pushed Git tag is exactly `v2.0.1`, matching the Cargo package
version, and that tag's commit is already contained in trusted `main`. Pull
requests, `main` pushes, and manual workflow runs build and exercise the image
but publish nothing, so they cannot overwrite a release image. The Docker build
context is deny-by-default and contains only the Cargo manifest/lockfile, Rust
source, policy schema, and pricing catalog. The release image records the exact
version and source revision and runs as the fixed unprivileged identity
`65532:65532`. Treat the version tag as immutable and pin the verified registry
digest for deployment; do not use `latest` as a release identity.

## Security boundaries

- Provider and Noveum keys are secrets. Do not log them, commit them, embed them
  in images, or pass them as command-line arguments.
- The gateway accepts credentials in request headers. Terminate TLS before any
  untrusted network and configure reverse proxies not to log sensitive headers.
- `DEBUG_METRICS=true` emits request and response bodies. Use it for controlled
  debugging, not general production logging of sensitive prompts.
- Provider-specific `RUST_LOG=debug` targets can also emit request bodies,
  responses, or streaming chunks. Keep production at `info`; enable debug only
  briefly with non-sensitive traffic and protected log storage.
- CORS is permissive. Browser exposure therefore requires an explicit
  authentication and origin-control layer in front of the gateway.
- Strict Nova Guard cost caps protect a policy budget; they are not a substitute
  for provider quotas, AWS IAM, network access control, or billing alerts.

## Contributing

See
[Contributing](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/CONTRIBUTING.md).
Before opening a pull request, run the applicable checks in
[Validation](https://github.com/Noveum/ai-gateway/blob/v2.0.1/docs/VALIDATION.md)
and update the changelog for user-visible behavior.

## License

Dual-licensed under
[MIT](https://github.com/Noveum/ai-gateway/blob/v2.0.1/LICENSE-MIT) or
[Apache License 2.0](https://github.com/Noveum/ai-gateway/blob/v2.0.1/LICENSE-APACHE),
at your option.
