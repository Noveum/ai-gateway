//! Nova Guard usage exporter — reports ALLOWED usage to the Noveum platform.
//!
//! Registered as a [`MetricsExporter`] only when platform-managed Nova Guard is
//! configured. For every *successful, non-blocked* proxied call it emits one
//! ALLOWED usage event (`costUsd` + token counts) so the platform's rolling
//! cost/rate counters advance — which is what lets a `cost_cap`/`rate_limit`
//! ever trip. BLOCKED events are emitted separately, from the guard middleware.
//!
//! **Not** the metering path in strict enforcement mode: there the platform's
//! reservation lifecycle (admit → complete/abandon) already records the call,
//! and reporting it here as well double-meters it. See [`AdmissionMetering`].
//!
//! Reporting is fire-and-forget: [`UsageReporter::report`] only enqueues, so this
//! exporter never adds latency to the request path.

use std::sync::Arc;

use crate::policy::admission::AdmissionClient;
use crate::policy::usage::{new_event_id, UsageEvent, UsageReporter};
use crate::policy::PolicyEngine;
use crate::telemetry::{metrics::MetricsExporter, RequestMetrics};
use async_trait::async_trait;
use std::error::Error;

/// Whether the platform's **reservation lifecycle** is the authoritative meter
/// for the traffic this gateway is currently serving.
///
/// In strict enforcement mode every guarded request takes a platform-side
/// reservation and settles it on the way out (`complete` with the real token
/// counts, `abandon` keeping the estimate). Each settlement *is* a metered
/// record, so the legacy `POST /policies/usage` report for the same call is a
/// second, independently-keyed record of one request — the platform cannot
/// collapse the two, and the org's spend counts double.
///
/// Evaluated live, never cached: the policy set is hot-swapped under a running
/// gateway by the `/effective` poller, so a cap can gain or lose
/// `enforcementMode: strict` between two requests. This mirrors the very same
/// call the guard middleware makes when it decides whether to admit through the
/// platform, so the two layers cannot disagree about who owns metering.
#[derive(Clone)]
pub struct AdmissionMetering {
    engine: Arc<PolicyEngine>,
    admission: Arc<AdmissionClient>,
}

impl AdmissionMetering {
    pub fn new(engine: Arc<PolicyEngine>, admission: Arc<AdmissionClient>) -> Self {
        Self { engine, admission }
    }

    /// True when this gateway's requests are metered by their reservations.
    ///
    /// Requires the guard middleware to be *running* on this traffic, not just
    /// configured: it fast-paths out (and so never admits, and never settles)
    /// when the engine is disabled or holds no active policies. Suppressing the
    /// exporter for requests nothing else meters would trade double-counted
    /// spend for uncounted spend — a worse bug, and a silent one.
    pub fn is_authoritative(&self) -> bool {
        self.engine.is_enabled()
            && self.engine.active_policy_count() > 0
            && self.admission.strict_for(self.engine.has_strict_cost_cap())
    }
}

pub struct NovaGuardUsagePlugin {
    reporter: UsageReporter,
    /// `None` when the deployment has no admission client at all (no platform
    /// bridge, or advisory-only): this exporter is then the *only* ALLOWED
    /// reporting path and always fires.
    admission: Option<AdmissionMetering>,
}

impl NovaGuardUsagePlugin {
    pub fn new(reporter: UsageReporter) -> Self {
        Self {
            reporter,
            admission: None,
        }
    }

    /// Wire the strict-mode gate. Once set, requests served while a strict
    /// `cost_cap` is active are metered by their reservation settlement instead
    /// of by this exporter (see [`AdmissionMetering`]).
    pub fn metered_by_admission(
        mut self,
        engine: Arc<PolicyEngine>,
        admission: Arc<AdmissionClient>,
    ) -> Self {
        self.admission = Some(AdmissionMetering::new(engine, admission));
        self
    }

    /// Whether this request should produce an ALLOWED usage event.
    ///
    /// Skips Nova Guard synthetic blocks (their BLOCKED event is reported by the
    /// middleware), non-2xx responses (a failed provider call didn't succeed —
    /// mirrors the SDK, which reports usage only after a successful call), and
    /// responses with no resolved model (the platform requires a model id).
    ///
    /// Also skips everything while the platform's reservation lifecycle is the
    /// authoritative meter — including the strict fail-open case, whose usage the
    /// guard middleware reports itself (it is the only layer that knows a request
    /// was never reserved).
    fn should_report(&self, m: &RequestMetrics) -> bool {
        if self
            .admission
            .as_ref()
            .is_some_and(AdmissionMetering::is_authoritative)
        {
            return false;
        }
        Self::is_metered_call(m)
    }

    /// The request-shape half of [`Self::should_report`], independent of which
    /// layer owns metering.
    fn is_metered_call(m: &RequestMetrics) -> bool {
        !m.guard_blocked && (200..300).contains(&m.status_code) && !m.model.trim().is_empty()
    }
}

#[async_trait]
impl MetricsExporter for NovaGuardUsagePlugin {
    async fn export_metrics(&self, metrics: RequestMetrics) -> Result<(), Box<dyn Error>> {
        if !self.should_report(&metrics) {
            return Ok(());
        }
        // A successful call reporting $0 means the platform's cost counters
        // won't advance for it — i.e. cost caps can't see this traffic. Make
        // that loudly observable (unknown model id missing from the pricing
        // table, or usage extraction failed).
        //
        // An INCOMPLETE breakdown is the case this used to miss entirely: a
        // call whose cached-read, cache-write or tool dimension the catalog
        // could not price. Those must never reach here as $0 — `price_usage`
        // charges a conservative upper bound instead — so a $0 incomplete cost
        // is a bug worth shouting about, and an incomplete cost at *any* amount
        // is worth naming the dimension for, because it is a bound rather than
        // a quote and cannot be reconciled against an invoice as-is.
        let breakdown = metrics.cost_breakdown.as_ref();
        if metrics.cost.unwrap_or(0.0) <= 0.0 {
            tracing::warn!(
                model = %metrics.model,
                input_tokens = ?metrics.input_tokens,
                output_tokens = ?metrics.output_tokens,
                pricing_version = ?breakdown.map(|b| b.pricing_version.as_str()),
                missing_dimensions = ?breakdown
                    .map(|b| b.missing_dimension_names())
                    .unwrap_or_default(),
                "Nova Guard: ALLOWED usage event carries $0 cost; this call is invisible to cost caps"
            );
        } else if let Some(b) = breakdown.filter(|b| !b.is_complete) {
            tracing::warn!(
                model = %metrics.model,
                pricing_version = %b.pricing_version,
                missing_dimensions = ?b.missing_dimension_names(),
                cost_usd = b.total_usd,
                "Nova Guard: ALLOWED usage event carries a CONSERVATIVE cost; a billable \
                 dimension could not be priced from the catalog and was charged at an upper bound"
            );
        }
        // Fresh event id per call; the reporter reuses it across retries so the
        // platform dedups. cost/tokens default to 0 when the provider body
        // carried no usage (unknown model, missing `usage` object, etc.).
        //
        // When a breakdown exists it — not `metrics.cost` — is the source of
        // the amount, and its components plus the catalog version ride along so
        // the platform can audit the number rather than take it on trust. The
        // breakdown's total already includes the conservative charge for any
        // dimension the catalog could not price, which is what makes an
        // incomplete cost impossible to post as a silent $0.
        let event = match metrics.cost_breakdown.clone() {
            Some(breakdown) => UsageEvent::allowed_with_breakdown(
                new_event_id(),
                metrics.model.clone(),
                breakdown,
                metrics.input_tokens.unwrap_or(0),
                metrics.output_tokens.unwrap_or(0),
            ),
            None => UsageEvent::allowed(
                new_event_id(),
                metrics.model.clone(),
                metrics.cost.unwrap_or(0.0),
                metrics.input_tokens.unwrap_or(0),
                metrics.output_tokens.unwrap_or(0),
            ),
        };
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
    use crate::policy::config::CostEnforcementMode;
    use crate::policy::engine::EngineOptions;
    use crate::policy::remote::RemoteConfig;
    use crate::policy::PolicyBundle;

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

    /// Points at a closed port: these tests never flush.
    fn test_cfg() -> RemoteConfig {
        RemoteConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "k".to_string(),
            project_id: "proj_test".to_string(),
        }
    }

    fn plugin() -> NovaGuardUsagePlugin {
        NovaGuardUsagePlugin::new(UsageReporter::spawn(test_cfg()))
    }

    /// A plugin whose gate is wired to an engine holding one `cost_cap` in the
    /// given enforcement mode.
    fn plugin_with_cap(mode: &str) -> NovaGuardUsagePlugin {
        let bundle = PolicyBundle::from_json_str(&format!(
            r#"{{"policies":[{{"name":"cap","type":"cost_cap","mode":"enforce","config":
               {{"window":"1d_rolling","maxUsd":10.0,"action":"block","enforcementMode":"{mode}"}}}}]}}"#
        ))
        .unwrap();
        let engine = Arc::new(PolicyEngine::from_bundle(
            &bundle,
            EngineOptions {
                live_state_backed: true,
                ..Default::default()
            },
        ));
        plugin().metered_by_admission(engine, Arc::new(AdmissionClient::new(test_cfg(), None)))
    }

    #[tokio::test]
    async fn reports_successful_calls() {
        assert!(plugin().should_report(&metrics(200, "gpt-4o", false)));
    }

    #[tokio::test]
    async fn skips_guard_blocks() {
        assert!(!plugin().should_report(&metrics(200, "gpt-4o", true)));
    }

    #[tokio::test]
    async fn skips_provider_errors() {
        assert!(!plugin().should_report(&metrics(500, "gpt-4o", false)));
        assert!(!plugin().should_report(&metrics(429, "gpt-4o", false)));
    }

    #[tokio::test]
    async fn skips_when_model_missing() {
        assert!(!plugin().should_report(&metrics(200, "", false)));
    }

    /// The double-metering guard: with a strict cap active, the reservation
    /// settlement is the record, so this exporter must stay silent.
    #[tokio::test]
    async fn skips_while_admission_meters_the_request() {
        assert!(!plugin_with_cap("strict").should_report(&metrics(200, "gpt-4o", false)));
    }

    /// ...and an advisory cap keeps the legacy path, which is the only one it has.
    #[tokio::test]
    async fn advisory_caps_keep_reporting_through_this_exporter() {
        assert!(plugin_with_cap("advisory").should_report(&metrics(200, "gpt-4o", false)));
    }

    /// The gate follows the *live* policy set and the deployment override, not a
    /// snapshot taken at startup: an operator who flips
    /// `NOVEUM_GUARD_COST_ENFORCEMENT=advisory` gets the legacy path back
    /// immediately, with no in-flight window where nothing meters at all.
    #[tokio::test]
    async fn the_deployment_override_wins_over_the_policy_set() {
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"cap","type":"cost_cap","mode":"enforce","config":
               {"window":"1d_rolling","maxUsd":10.0,"action":"block","enforcementMode":"strict"}}]}"#,
        )
        .unwrap();
        let engine = Arc::new(PolicyEngine::from_bundle(
            &bundle,
            EngineOptions {
                live_state_backed: true,
                ..Default::default()
            },
        ));
        let forced_advisory = plugin().metered_by_admission(
            engine,
            Arc::new(AdmissionClient::new(
                test_cfg(),
                Some(CostEnforcementMode::Advisory),
            )),
        );
        assert!(forced_advisory.should_report(&metrics(200, "gpt-4o", false)));
    }

    /// No admission client at all (no platform bridge / advisory deployment):
    /// unchanged behavior, this exporter is the only reporting path.
    #[tokio::test]
    async fn an_ungated_plugin_always_reports() {
        assert!(plugin().should_report(&metrics(200, "gpt-4o", false)));
    }

    /// `NOVEUM_GUARD_COST_ENFORCEMENT=strict` with an empty policy set: the
    /// guard middleware fast-paths out, so no reservation is ever taken and this
    /// exporter is the only meter there is. Going quiet here would stop the
    /// platform's counters (and its spend dashboards) outright.
    #[tokio::test]
    async fn an_empty_policy_set_keeps_the_legacy_path_even_when_forced_strict() {
        let engine = Arc::new(PolicyEngine::from_bundle(
            &PolicyBundle::from_json_str(r#"{"policies":[]}"#).unwrap(),
            EngineOptions {
                live_state_backed: true,
                ..Default::default()
            },
        ));
        let forced_strict = plugin().metered_by_admission(
            engine,
            Arc::new(AdmissionClient::new(
                test_cfg(),
                Some(CostEnforcementMode::Strict),
            )),
        );
        assert!(forced_strict.should_report(&metrics(200, "gpt-4o", false)));
    }

    // -- itemized cost on the usage event ---------------------------------

    use crate::policy::pricing::{price_usage, BillableDimension, BillableUsage, CATALOG_VERSION};

    fn with_breakdown(model: &str, usage: BillableUsage) -> RequestMetrics {
        let breakdown = price_usage(model, &usage);
        RequestMetrics {
            model: model.to_string(),
            status_code: 200,
            input_tokens: Some(usage.total_input_tokens()),
            output_tokens: Some(usage.output_tokens),
            cost: Some(breakdown.total_usd),
            cost_breakdown: Some(breakdown),
            ..Default::default()
        }
    }

    /// The event the exporter would post, as the platform would see it.
    fn posted(m: &RequestMetrics) -> serde_json::Value {
        let event = match m.cost_breakdown.clone() {
            Some(b) => UsageEvent::allowed_with_breakdown(
                new_event_id(),
                m.model.clone(),
                b,
                m.input_tokens.unwrap_or(0),
                m.output_tokens.unwrap_or(0),
            ),
            None => UsageEvent::allowed(
                new_event_id(),
                m.model.clone(),
                m.cost.unwrap_or(0.0),
                m.input_tokens.unwrap_or(0),
                m.output_tokens.unwrap_or(0),
            ),
        };
        serde_json::to_value(event).unwrap()
    }

    #[tokio::test]
    async fn an_allowed_event_carries_the_components_and_the_catalog_version() {
        let m = with_breakdown(
            "claude-sonnet-4-5",
            BillableUsage {
                uncached_input_tokens: 10_000,
                cache_read_tokens: 90_000,
                cache_write_tokens: 5_000,
                output_tokens: 1_000,
                tool_calls: vec![("anthropic:web_search".to_string(), 2)],
                ..Default::default()
            },
        );
        let v = posted(&m);
        assert_eq!(v["pricingVersion"], CATALOG_VERSION);
        let b = &v["costBreakdown"];
        assert_eq!(b["isComplete"], true);
        assert_eq!(b["source"], "CATALOG");
        for key in [
            "uncachedInputUsd",
            "cacheReadUsd",
            "cacheWriteUsd",
            "outputUsd",
            "toolUsd",
        ] {
            assert!(b[key].as_f64().unwrap() > 0.0, "{key} must be itemized");
        }
        // The posted total is the breakdown's, and the components explain it.
        let sum: f64 = [
            "uncachedInputUsd",
            "cacheReadUsd",
            "cacheWriteUsd",
            "outputUsd",
            "toolUsd",
        ]
        .iter()
        .map(|k| b[*k].as_f64().unwrap())
        .sum();
        assert!((sum - v["costUsd"].as_f64().unwrap()).abs() < 1e-12);
    }

    /// The invariant the whole change exists for: a call whose cost the catalog
    /// could not fully compute must not reach the platform as a free one.
    #[tokio::test]
    async fn an_incomplete_breakdown_cannot_post_a_silent_zero() {
        // o1 reports cached tokens; the pricing page publishes no cached rate.
        let m = with_breakdown(
            "o1",
            BillableUsage {
                cache_read_tokens: 100_000,
                ..Default::default()
            },
        );
        let breakdown = m.cost_breakdown.as_ref().unwrap();
        assert!(!breakdown.is_complete);
        assert_eq!(
            breakdown.missing_dimensions,
            vec![BillableDimension::CacheRead]
        );

        let v = posted(&m);
        assert!(
            v["costUsd"].as_f64().unwrap() > 0.0,
            "an unpriceable dimension must never post as $0"
        );
        assert_eq!(v["costBreakdown"]["isComplete"], false);
        assert_eq!(v["costBreakdown"]["missingDimensions"][0], "CACHE_READ");
        assert!(v["costBreakdown"]["cacheReadUsd"].as_f64().unwrap() > 0.0);
        // The platform can tell this apart from a confidently-priced call.
        let priced = posted(&with_breakdown(
            "claude-sonnet-4-5",
            BillableUsage {
                cache_read_tokens: 100_000,
                ..Default::default()
            },
        ));
        assert_eq!(priced["costBreakdown"]["isComplete"], true);
        assert!(priced["costBreakdown"]["missingDimensions"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// A cached read is billed at the cached rate, so the platform's counters
    /// advance by what was actually spent rather than by the input rate.
    #[tokio::test]
    async fn a_cached_read_posts_less_than_the_same_tokens_as_fresh_input() {
        let cached = posted(&with_breakdown(
            "claude-sonnet-4-5",
            BillableUsage {
                uncached_input_tokens: 10_000,
                cache_read_tokens: 90_000,
                output_tokens: 1_000,
                ..Default::default()
            },
        ));
        let fresh = posted(&with_breakdown(
            "claude-sonnet-4-5",
            BillableUsage {
                uncached_input_tokens: 100_000,
                output_tokens: 1_000,
                ..Default::default()
            },
        ));
        assert!(cached["costUsd"].as_f64().unwrap() < fresh["costUsd"].as_f64().unwrap());
        // Both still report the same token counts to the platform.
        assert_eq!(cached["inputTokens"], fresh["inputTokens"]);
    }

    /// Metrics with no breakdown at all (a call with no usage to price) keep the
    /// pre-existing bare-total shape rather than posting an empty breakdown.
    #[tokio::test]
    async fn a_call_with_nothing_to_price_posts_no_breakdown() {
        let m = RequestMetrics {
            model: "gpt-4o".to_string(),
            status_code: 200,
            cost: Some(0.01),
            ..Default::default()
        };
        let v = posted(&m);
        assert!(v.get("costBreakdown").is_none());
        assert!(v.get("pricingVersion").is_none());
        assert_eq!(v["costUsd"], 0.01);
    }
}
