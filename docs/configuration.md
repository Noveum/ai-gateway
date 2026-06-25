# Configuration

Noveum AI Gateway is configured entirely through environment variables, read once
at startup. The full, authoritative list lives in the
[README "Configuration" section](../README.md#configuration-environment-variables);
this page summarizes the groups.

## Server / runtime
- `PORT` (default `3000`) — listen port
- `HOST` (default `127.0.0.1`) — bind address
- `WORKER_THREADS` (default: derived from CPU cores) — Tokio worker threads
- `MAX_CONNECTIONS` (default `10000`) — max idle HTTP connections per upstream host
- `RUST_LOG` (default `info`) — tracing filter

## Telemetry (optional)
- `DEBUG_METRICS` (default `false`) — console metrics exporter
- `DEPLOYMENT_ENVIRONMENT` (default `development`) — resource tag

  Add new exporters by implementing `MetricsExporter`; see
  [telemetry-plugins.md](telemetry-plugins.md).

## Nova Guard (optional)
- `NOVEUM_GUARD_ENABLED`, `NOVEUM_GUARD_POLICIES_FILE`, `NOVEUM_GUARD_POLICIES`,
  `NOVEUM_GUARD_BLOCK_RESPONSE_MODE`. See [NOVA_GUARD.md](NOVA_GUARD.md).

## Per-request headers
Provider selection and attribution are per request, via headers:
- `x-provider` — required; selects the upstream (e.g. `openai`, `anthropic`, `groq`, …)
- `Authorization: Bearer <key>` — the upstream provider's API key (Bedrock uses
  `x-aws-access-key-id` / `x-aws-secret-access-key` / `x-aws-region`)
- `x-project-id`, `x-organization-id`, `x-user-id`, `x-experiment-id` — attribution
  tags captured into the per-request metrics
