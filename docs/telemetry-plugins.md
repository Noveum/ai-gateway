# Telemetry Exporters Guide

This guide explains how per-request telemetry flows through the gateway and how to
add a new exporter.

## Overview

For every proxied request the gateway builds a [`RequestMetrics`] value (provider,
model, token usage, cost, latency, status, request/response bodies, and tracking
ids from headers). The [`MetricsRegistry`] fans that value out to every registered
[`MetricsExporter`] concurrently, so adding a destination is just implementing one
trait and registering it at startup.

- `RequestMetrics` and the serializers (`to_otel_log`, `to_noveum_trace`) —
  `src/telemetry/mod.rs`
- `MetricsExporter` trait + `MetricsRegistry` — `src/telemetry/metrics.rs`
- Built-in exporters:
  - **Console** (`src/telemetry/plugins/console.rs`) — pretty-prints metrics for
    local debugging. Enabled with `DEBUG_METRICS=true`.
  - **Noveum trace** (`src/telemetry/exporters/noveum_trace.rs`) — ships traffic to
    the Noveum platform as a span-based trace batch. Enabled with
    `ENABLE_NOVEUM_TRACES=true` plus `NOVEUM_ENDPOINT` and `NOVEUM_API_KEY`.

## The `MetricsExporter` trait

```rust
#[async_trait]
pub trait MetricsExporter: Send + Sync {
    /// Ship one request's metrics. Errors are logged by the registry and never
    /// block the proxied response.
    async fn export_metrics(&self, metrics: RequestMetrics) -> Result<(), Box<dyn std::error::Error>>;

    /// Stable name for logging.
    fn name(&self) -> &str;
}
```

Exporters run off the request hot path, so a slow or failing exporter degrades
telemetry only — never the proxied request.

## Adding a new exporter

1. **Create the module** under `src/telemetry/exporters/` (e.g. `datadog.rs`) and
   declare it in `src/telemetry/exporters/mod.rs`.

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
           // metrics.to_noveum_trace(project, env) and metrics.to_otel_log() are
           // available if a span/otel shape helps.
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

4. **Test it** with a `wiremock` mock server, asserting the request shape and that
   failures surface as `Err` (see `noveum_trace.rs` tests for the pattern).

## Project / org attribution

`RequestMetrics` carries `project_id`, `org_id`, `user_id`, and `experiment_id`,
populated from the `x-project-id`, `x-organization-id`, `x-user-id`, and
`x-experiment-id` request headers. Exporters should attribute data using these.

[`RequestMetrics`]: ../src/telemetry/mod.rs
[`MetricsRegistry`]: ../src/telemetry/metrics.rs
[`MetricsExporter`]: ../src/telemetry/metrics.rs
