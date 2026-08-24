# Telemetry exporter development

This guide explains how per-request telemetry flows through the gateway and how to
add a new exporter.

## Overview

For every proxied request the native gateway builds a [`RequestMetrics`] value
(provider, model, token usage, cost, latency, status, request/response bodies,
and tracking IDs from headers). The [`MetricsRegistry`] spawns an independent
export task for each registered [`MetricsExporter`]. Export failures are logged
and do not change the already-proxied response.

- `RequestMetrics` and the `to_otel_log` serializer — `src/telemetry/mod.rs`
- `MetricsExporter` trait + `MetricsRegistry` — `src/telemetry/metrics.rs`
- Built-in exporters:
  - **Console** (`src/telemetry/plugins/console.rs`) — pretty-prints metrics for
    local debugging. Enabled with `DEBUG_METRICS=true`.

Shipping records to an external observability backend requires implementing
`MetricsExporter` and registering it in `main.rs` as described below. The
Cloudflare Worker does not run these native exporters.

## The `MetricsExporter` trait

```rust
#[async_trait]
pub trait MetricsExporter: Send + Sync {
    /// Ship one request's metrics. Errors are logged by the registry and never
    /// block the proxied response.
    async fn export_metrics(
        &self,
        metrics: RequestMetrics,
    ) -> Result<(), Box<dyn std::error::Error>>;

    /// Stable name for logging.
    fn name(&self) -> &str;
}
```

Exporter tasks run after recording is scheduled, so a slow or failing exporter
degrades telemetry only. Add explicit timeouts, bounded queues, retry limits,
and shutdown behavior when delivery guarantees matter; the trait itself does
not provide them.

## Adding a new exporter

1. **Create the module** under `src/telemetry/plugins/` (e.g. `datadog.rs`,
   alongside `console.rs`) and declare it in `src/telemetry/plugins/mod.rs`.

2. **Implement `MetricsExporter`.** Use one of the provided serializers, or read
   fields off `RequestMetrics` directly:

   ```rust
   use crate::telemetry::metrics::MetricsExporter;
   use crate::telemetry::RequestMetrics;
   use async_trait::async_trait;

   pub struct DatadogExporter { /* client, api key, ... */ }

   #[async_trait]
   impl MetricsExporter for DatadogExporter {
       async fn export_metrics(&self, metrics: RequestMetrics) -> Result<(), Box<dyn std::error::Error>> {
           // Map RequestMetrics -> your wire format and POST it.
           // metrics.to_otel_log() is available if an OTel-log shape helps.
           Ok(())
       }
       fn name(&self) -> &str { "datadog" }
   }
   ```

3. **Register it at startup** in `src/main.rs`, gated behind an env flag, next to
   the existing exporters:

   ```rust
   if std::env::var("ENABLE_DATADOG").is_ok() {
       metrics_registry.register_exporter(Box::new(DatadogExporter::new(/* ... */))).await;
   }
   ```

4. **Test it** with a mock HTTP server (e.g. `wiremock`), asserting the request
   shape and that failures surface as `Err`.

## Project / org attribution

`RequestMetrics` carries `project_id`, `org_id`, `user_id`, and `experiment_id`.
In transparent and dedicated modes these begin as caller-supplied attribution,
not authenticated identity. Native shared tenancy validates project and
organization against the tenant derived from `x-noveum-api-key`. Exporters must
preserve that distinction.

Request and response bodies can contain prompts, tool arguments, personal data,
and confidential output. Apply minimization/redaction before external export,
keep provider/Noveum/AWS credentials out of records, and define retention and
access controls. See [Telemetry and log handling](logs.md).

[`RequestMetrics`]: ../src/telemetry/mod.rs
[`MetricsRegistry`]: ../src/telemetry/metrics.rs
[`MetricsExporter`]: ../src/telemetry/metrics.rs
