//! Nova Guard usage exporter — reports ALLOWED usage to the Noveum platform.
//!
//! Registered as a [`MetricsExporter`] only when platform-managed Nova Guard is
//! configured. For every *successful, non-blocked* proxied call it emits one
//! ALLOWED usage event (`costUsd` + token counts) so the platform's rolling
//! cost/rate counters advance — which is what lets a `cost_cap`/`rate_limit`
//! ever trip. BLOCKED events are emitted separately, from the guard middleware.
//!
//! Reporting is fire-and-forget: [`UsageReporter::report`] only enqueues, so this
//! exporter never adds latency to the request path.

use crate::policy::usage::{new_event_id, UsageEvent, UsageReporter};
use crate::telemetry::{metrics::MetricsExporter, RequestMetrics};
use async_trait::async_trait;
use std::error::Error;

pub struct NovaGuardUsagePlugin {
    reporter: UsageReporter,
}

impl NovaGuardUsagePlugin {
    pub fn new(reporter: UsageReporter) -> Self {
        Self { reporter }
    }

    /// Whether this request should produce an ALLOWED usage event.
    ///
    /// Skips Nova Guard synthetic blocks (their BLOCKED event is reported by the
    /// middleware), non-2xx responses (a failed provider call didn't succeed —
    /// mirrors the SDK, which reports usage only after a successful call), and
    /// responses with no resolved model (the platform requires a model id).
    fn should_report(m: &RequestMetrics) -> bool {
        !m.guard_blocked && (200..300).contains(&m.status_code) && !m.model.trim().is_empty()
    }
}

#[async_trait]
impl MetricsExporter for NovaGuardUsagePlugin {
    async fn export_metrics(&self, metrics: RequestMetrics) -> Result<(), Box<dyn Error>> {
        if !Self::should_report(&metrics) {
            return Ok(());
        }
        // Fresh event id per call; the reporter reuses it across retries so the
        // platform dedups. cost/tokens default to 0 when the provider body
        // carried no usage (unknown model, missing `usage` object, etc.).
        let event = UsageEvent::allowed(
            new_event_id(),
            metrics.model.clone(),
            metrics.cost.unwrap_or(0.0),
            metrics.input_tokens.unwrap_or(0),
            metrics.output_tokens.unwrap_or(0),
        );
        self.reporter.report(event);
        Ok(())
    }

    fn name(&self) -> &str {
        "nova_guard_usage"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(status: u16, model: &str, blocked: bool) -> RequestMetrics {
        RequestMetrics {
            model: model.to_string(),
            status_code: status,
            guard_blocked: blocked,
            cost: Some(0.01),
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        }
    }

    #[test]
    fn reports_successful_calls() {
        assert!(NovaGuardUsagePlugin::should_report(&metrics(
            200, "gpt-4o", false
        )));
    }

    #[test]
    fn skips_guard_blocks() {
        assert!(!NovaGuardUsagePlugin::should_report(&metrics(
            200, "gpt-4o", true
        )));
    }

    #[test]
    fn skips_provider_errors() {
        assert!(!NovaGuardUsagePlugin::should_report(&metrics(
            500, "gpt-4o", false
        )));
        assert!(!NovaGuardUsagePlugin::should_report(&metrics(
            429, "gpt-4o", false
        )));
    }

    #[test]
    fn skips_when_model_missing() {
        assert!(!NovaGuardUsagePlugin::should_report(&metrics(
            200, "", false
        )));
    }
}
