# Configuration and credentials

The native gateway reads environment variables at startup. The Cloudflare
Worker reads equivalent Wrangler vars/secrets per isolate; filesystem policy
paths and native-only settings do not apply there.

## Choose one mode

| Mode | Required settings | Forbidden settings | Runtime |
|---|---|---|---|
| Transparent | none | none | native or Worker |
| Local policies | `NOVEUM_GUARD_POLICIES_FILE` or `NOVEUM_GUARD_POLICIES`; Worker may also use KV (`NOVEUM_GUARD_POLICIES_KV`, key `nova-guard-policies`) | `NOVEUM_GUARD_TENANCY=shared`; do not also configure the dedicated platform pair | file: native only; inline/KV: Worker |
| Platform dedicated | `NOVEUM_API_KEY`, `NOVEUM_GUARD_PROJECT_ID`; optionally `NOVEUM_GUARD_TENANCY=dedicated` | `NOVEUM_GUARD_TENANCY=shared` | native or Worker |
| Platform shared | `NOVEUM_GUARD_TENANCY=shared`; caller sends `x-noveum-api-key` | process-wide `NOVEUM_API_KEY`, `NOVEUM_GUARD_PROJECT_ID`, and local bundle | native only |

An unset tenancy with a complete Noveum key/project pair means dedicated mode
for backward compatibility. Shared mode is never inferred. An incomplete pair,
blank value, unknown tenancy, or incompatible combination is an error rather
than a silent pass-through. The native gateway validates this at startup. The
Worker evaluates runtime bindings per proxy request, so `/` and `/health` can
remain available while `/v1/*` returns a configuration error.

Local and platform policy sources are alternatives, not additive layers. When a
complete dedicated bridge is present, the Noveum control plane is the policy
source; an inline/local bundle is not combined with it. Remove local-source
settings from a dedicated deployment so the intended source is unambiguous.

The production Cloudflare hostname `gate.noveum.ai` is transparent because it
is shared by callers. Binding it to one dedicated project would attribute every
call to that project. Use one dedicated Worker/domain per project, or the native
shared mode, when platform-managed enforcement is required.

## Server and telemetry (native)

| Variable | Default | Meaning |
|---|---|---|
| `HOST` | `127.0.0.1` | Bind address. Use `0.0.0.0` in a container. |
| `PORT` | `3000` | Listen port. Must be a valid `u16`. |
| `WORKER_THREADS` | derived from CPU count | Positive Tokio runtime worker-thread count. Zero prevents startup. |
| `MAX_CONNECTIONS` | `10000` | Maximum idle HTTP connections retained per upstream host. |
| `RUST_LOG` | `info` | Tracing filter, for example `noveum_ai_gateway=debug`. |
| `DEBUG_METRICS` | `false` | Registers the console metrics exporter. Output can include prompt and response bodies. |
| `DEPLOYMENT_ENVIRONMENT` | `development` | `deployment.environment` field in exported telemetry. |

Version 2.0.1 honors both `HOST` and `WORKER_THREADS`: it builds the Tokio
runtime with the configured worker count before startup. Version 2.0.0 read
these settings but had startup bugs that left the listener on the wildcard
address and the Tokio runtime at its previously created worker count. Apply an
external network boundary while upgrading a v2.0.0 deployment.

Configuration is read once. Restart or redeploy after changing it.

## Nova Guard policy source

| Variable | Default | Meaning |
|---|---|---|
| `NOVEUM_GUARD_ENABLED` | `true` | Master switch. `false`, `0`, `no`, `off`, `disabled`, or an empty value disables the engine. |
| `NOVEUM_GUARD_POLICIES_FILE` | unset | Native-only path to a JSON policy bundle. |
| `NOVEUM_GUARD_POLICIES` | unset | Inline JSON policy bundle; on Workers, store sensitive bundles as a secret. Fallback when KV is bound but the key is absent. |
| `NOVEUM_GUARD_POLICIES_KV` | unset | Worker-only KV namespace binding. Fixed key `nova-guard-policies`. Optional; default deploy has no bindings. |
| `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` | `synthetic_success` | `synthetic_success` returns a refusal-shaped HTTP 200; `provider_error` returns an HTTP 403 error envelope. |
| `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` | `1024` | Fallback estimate for advisory cost caps and rate-only accounting; never satisfies an applicable strict cost cap's explicit-bound requirement. |

A configured but unreadable, malformed, future-major, or unenforceable local
bundle prevents native startup. The Worker evaluates its inline binding per
proxy request, so its information and health routes can remain available while
`/v1/*` returns a configuration error. With no policy source and no platform
bridge, the gateway is a transparent proxy.

## Platform bridge

| Variable | Default | Meaning |
|---|---|---|
| `NOVEUM_API_KEY` | unset | Dedicated deployment service key. Store as a secret. Must be absent in shared mode. |
| `NOVEUM_GUARD_PROJECT_ID` | unset | Fixed dedicated project. Must be absent in shared mode. |
| `NOVEUM_API_URL` | `https://api.noveum.ai` | Noveum control-plane base URL. |
| `NOVEUM_GUARD_TENANCY` | inferred dedicated or transparent | Explicit `dedicated` or `shared`. Worker refuses `shared`. |
| `NOVEUM_GUARD_COST_ENFORCEMENT` | policy value | Native-only `strict` or `advisory` deployment override. `strict` requires the platform bridge. Worker always uses each policy's `enforcementMode`. |
| `NOVEUM_GUARD_ALLOW_UNGUARDED_START` | `false` | Emergency-only escape hatch when the first policy fetch fails. Traffic is unguarded until a later fetch succeeds. |
| `NOVEUM_GUARD_TENANT_TTL_SECS` | `300` | Shared native mode: credential-to-tenant resolution TTL. |
| `NOVEUM_GUARD_TENANT_CACHE_MAX` | `1024` | Shared native mode: maximum warm tenant runtimes. |
| `NOVEUM_GUARD_USAGE_FLUSH_MS` | `2000` | Native usage batching interval. Primarily a test/diagnostic knob; avoid unnecessary production tuning. |
| `NOVEUM_GUARD_WORKER_UPSTREAM_TIMEOUT_MS` | `600000` | Worker-only admitted provider deadline. May be lowered but cannot exceed 10 minutes. |

### Required key scopes

| Scope | Dedicated service key | Shared caller key |
|---|:---:|:---:|
| `guardrails:read` | required | required |
| `guardrails:ingest` | required | required |
| `projects:read` | not required | required |

The dedicated service key is process-wide. The shared caller key arrives in
`x-noveum-api-key`, is used only for that derived tenant, and is removed before
provider dispatch.

## Provider base URL overrides

| Variable | Default | Meaning |
|---|---|---|
| `OPENAI_BASE_URL` | `https://api.openai.com` | Override only the literal `x-provider: openai` upstream. |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Override the Anthropic origin; the adapter still appends `/v1/messages`. |

Leave these unset in production unless the target is an intentional compatible
proxy. An empty or whitespace-only override is treated as unset and uses the
default URL. Other provider routes use their compiled upstream URL in the 2.0
line.

## Per-request headers

| Header | Meaning |
|---|---|
| `x-provider` | Optional provider selector, such as `openai`, `anthropic`, `groq`, `fireworks`, `together`, or `bedrock`; absence defaults to OpenAI. |
| `Authorization: Bearer …` | Provider credential for OpenAI-style routes; Anthropic also accepts `x-api-key`. |
| `x-aws-access-key-id`, `x-aws-secret-access-key`, `x-aws-region` | Bedrock credential and source Region. |
| `x-aws-session-token` | Optional temporary Bedrock credential token on native and Worker runtimes. |
| `x-noveum-api-key` | Native shared-tenancy caller credential; never a provider credential. |
| `x-project-id` | Attribution, or an entitled-project selector in shared mode. Not authentication. |
| `x-organization-id` / `x-organisation-id` | Attribution, or a shared-mode confirmation that must match the derived organization. |
| `x-user-id`, `x-experiment-id` | Optional telemetry attribution. |

Provider keys and AWS credential headers must travel only over TLS to a gateway
you control. Configure access logs and reverse proxies to redact them.

## Secret storage

- Native process/container: inject secrets from the platform secret manager;
  keep `.env` files out of images and source control.
- Kubernetes: reference a Secret or an external secret provider rather than
  placing values in a Deployment manifest.
- Cloudflare: source values from an approved secret manager and stage production
  changes with `wrangler versions secret` plus the versioned promotion flow.
  The ordinary `wrangler secret put` deploys immediately. Never commit a secret
  to `[vars]`, pass its value as a command argument, or print it in CI.

See [Nova Guard](NOVA_GUARD.md) for policy semantics and
[Cloudflare Worker operations](CLOUDFLARE_WORKER.md) for Worker-specific setup.
