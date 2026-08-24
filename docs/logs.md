# Telemetry and log handling

The native gateway builds one `RequestMetrics` record for each proxied request.
It includes provider/model identity, timing, sizes, status codes, token usage,
estimated cost, attribution fields, and request/response bodies. Registered
`MetricsExporter` plugins receive independent copies asynchronously.

The built-in console exporter is disabled by default. Enable it only for a
controlled diagnostic session:

```bash
DEBUG_METRICS=true DEPLOYMENT_ENVIRONMENT=development RUST_LOG=info \
  noveum-ai-gateway
```

It prints both the Rust debug representation and an OpenTelemetry-compatible
JSON log. See [Telemetry exporters](telemetry-plugins.md) to implement a durable
sink.

## Record shape

Fields are omitted or `null` when a provider does not report them. This is a
representative, abbreviated record; version and values come from the running
request:

```json
{
  "timestamp": "2026-08-24T12:00:00Z",
  "resource": {
    "service.name": "noveum_ai_gateway",
    "service.version": "2.0.1",
    "deployment.environment": "production"
  },
  "name": "ai_gateway_request_log",
  "attributes": {
    "id": "msg_1234abcd",
    "thread_id": "thread_1234abcd",
    "org_id": "org_example",
    "project_id": "project_example",
    "user_id": "user_example",
    "experiment_id": "experiment_example",
    "provider": "openai",
    "model": "gpt-4o-mini",
    "request": {
      "model": "gpt-4o-mini",
      "messages": [{"role": "user", "content": "Example prompt"}],
      "max_tokens": 32
    },
    "response": {
      "choices": [{"message": {"role": "assistant", "content": "Example response"}}]
    },
    "metadata": {
      "latency": 420,
      "ttfb": 190,
      "provider_latency": 400,
      "tokens": {"input": 12, "output": 4, "total": 16},
      "cost": 0.0000042,
      "pricing_version": "2026.08.23",
      "status": "success",
      "path": "/v1/chat/completions",
      "method": "POST",
      "request_size": 152,
      "response_size": 244,
      "status_code": 200,
      "provider_status_code": 200,
      "error_count": 0,
      "error_type": null,
      "provider_error_count": 0,
      "provider_error_type": null,
      "provider_request_id": "provider-request-id"
    }
  }
}
```

When cost can be computed, `metadata.cost_breakdown` also itemizes uncached
input, cache reads, cache writes, output, tool fees, total, completeness,
missing dimensions, pricing version, whether an unknown-model assumption was
used, and whether the value came from catalog arithmetic or an authoritative
provider total. See [Pricing](PRICING.md) for the accounting contract.

## Attribution headers

The native middleware reads:

- `x-project-id`
- `x-organization-id` (and the `x-organisation-id` spelling where supported)
- `x-user-id`
- `x-experiment-id`

These values are caller input, not trusted identity in transparent/dedicated
mode. In native shared tenancy, project and organization are checked against the
tenant derived from `x-noveum-api-key` before policy enforcement. Exporters
should preserve that distinction.

## Sensitive-data warning

`DEBUG_METRICS=true` can print full prompts, model responses, tool arguments,
and streaming chunks. Provider-specific `RUST_LOG=debug` targets can also log
request bodies, response JSON, or stream chunks during ordinary tracing. The
serializer normalizes some JSON shapes; it does **not** anonymize or remove
confidential content. Therefore:

- keep the console exporter off in normal production;
- keep `RUST_LOG=info` in production and use narrowly targeted debug filters
  only for brief, controlled sessions with protected log storage;
- apply minimization/redaction before sending records to an external sink;
- set retention and access controls appropriate for prompt content;
- never emit provider, Noveum, or AWS credential headers;
- configure ingress, reverse-proxy, APM, and Cloudflare logs to redact sensitive
  headers independently; and
- use synthetic, non-sensitive prompts during production probes.

The built-in metrics record does not serialize request headers, but that does
not prevent another proxy or debug layer from logging them.

## Operational signals

At minimum collect and alert on:

- gateway and provider status code/error type;
- total latency, provider latency, and TTFB;
- restarts, panics, and sustained 5xx;
- missing or incomplete token/cost dimensions;
- Nova Guard blocks and policy/configuration rejections;
- platform state/admission failures, especially fail-closed blocks;
- abandoned reservations and settlement retry exhaustion; and
- usage queue overflow/drop warnings.

The Cloudflare Worker does not run native `MetricsExporter` plugins. Use
`wrangler tail` for real-time diagnostics and enable Workers Logs (or an
approved export) for persistent edge observability; see the
[Worker runbook](CLOUDFLARE_WORKER.md#observability).
