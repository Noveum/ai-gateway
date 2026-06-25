//! The Nova Guard policy engine.
//!
//! The engine compiles a [`PolicyBundle`] into runnable rules, orders them by
//! priority, and evaluates them per phase. It applies mode semantics
//! (off/shadow/enforce), composes transforms (each policy sees the previous
//! policy's mutated text), and short-circuits on the first enforced block.
//!
//! State is held behind an [`arc_swap::ArcSwap`] so a background refresh task
//! (control-plane polling) can hot-swap the active policy set without locking
//! the request hot path.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use tracing::{info, warn};

use super::config::{
    CostCapConfig, CostEnforcementMode, CostWindow, Policy, PolicyBundle, PolicyType,
    RateLimitConfig,
};
use super::decision::{Phase, PolicyAction, PolicyDecision, PolicyMode, Severity};
use super::rules::{compile_rule, EvalContext, LiveState, PolicyRule, RuleOutcome};
use super::synthetic::BlockResponseMode;

/// Per-policy metadata carried alongside the compiled rule.
struct PolicyMeta {
    id: String,
    name: String,
    policy_type: PolicyType,
    mode: PolicyMode,
    fail_closed: bool,
}

impl PolicyMeta {
    fn from_policy(p: &Policy) -> Self {
        Self {
            id: p.id().to_string(),
            name: p.name.clone(),
            policy_type: p.policy_type,
            mode: p.mode,
            fail_closed: p.fail_closed,
        }
    }
}

/// A compiled text rule plus its policy metadata.
struct CompiledPolicy {
    meta: PolicyMeta,
    rule: Box<dyn PolicyRule>,
}

/// A cost-cap policy (handled via live-state, not as a text rule).
struct CostCapPolicy {
    meta: PolicyMeta,
    config: CostCapConfig,
}

/// A rate-limit policy (handled via live-state).
struct RateLimitPolicy {
    meta: PolicyMeta,
    config: RateLimitConfig,
}

/// Immutable snapshot of the active policy set. Swapped atomically on refresh.
pub struct EngineState {
    enabled: bool,
    text_policies: Vec<CompiledPolicy>,
    cost_caps: Vec<CostCapPolicy>,
    rate_limits: Vec<RateLimitPolicy>,
}

impl EngineState {
    fn active_count(&self) -> usize {
        self.text_policies.len() + self.cost_caps.len() + self.rate_limits.len()
    }
}

/// The result of evaluating one phase.
#[derive(Debug, Default)]
pub struct EvaluationResult {
    /// Every decision made this phase (including shadow and allow), for telemetry.
    pub decisions: Vec<PolicyDecision>,
    /// Final payload text after all applied transforms, if any policy transformed it.
    pub transformed_text: Option<String>,
    /// The decision that blocked the call, if any.
    pub block: Option<PolicyDecision>,
}

impl EvaluationResult {
    pub fn is_blocked(&self) -> bool {
        self.block.is_some()
    }
}

/// Options controlling engine-wide behavior.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub enabled: bool,
    pub block_mode: BlockResponseMode,
    /// When live state is unavailable, fail open (allow) unless a policy opts into
    /// fail-closed.
    pub fail_open_default: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            block_mode: BlockResponseMode::SyntheticSuccess,
            fail_open_default: true,
        }
    }
}

pub struct PolicyEngine {
    state: ArcSwap<EngineState>,
    block_mode: BlockResponseMode,
    fail_open_default: bool,
}

impl PolicyEngine {
    /// Build an engine from a bundle and options.
    pub fn from_bundle(bundle: &PolicyBundle, opts: EngineOptions) -> Self {
        let state = Self::compile(bundle, opts.enabled);
        Self {
            state: ArcSwap::from_pointee(state),
            block_mode: opts.block_mode,
            fail_open_default: opts.fail_open_default,
        }
    }

    /// A fully disabled, no-op engine that allows everything.
    pub fn disabled() -> Self {
        Self::from_bundle(
            &PolicyBundle::default(),
            EngineOptions {
                enabled: false,
                ..Default::default()
            },
        )
    }

    /// Build an engine from environment configuration.
    ///
    /// * `NOVEUM_GUARD_ENABLED` (default `true`)
    /// * `NOVEUM_GUARD_POLICIES_FILE` — path to a `nova-guard.json` bundle
    /// * `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` — `synthetic_success` | `provider_error`
    ///
    /// Any load/parse error degrades to an empty (pass-through) engine with a
    /// warning; the gateway never fails to boot because of policy config.
    pub async fn from_env() -> Self {
        let enabled = std::env::var("NOVEUM_GUARD_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let block_mode = std::env::var("NOVEUM_GUARD_BLOCK_RESPONSE_MODE")
            .map(|v| BlockResponseMode::from_env_str(&v))
            .unwrap_or(BlockResponseMode::SyntheticSuccess);

        let opts = EngineOptions {
            enabled,
            block_mode,
            fail_open_default: true,
        };

        if !enabled {
            return Self::from_bundle(&PolicyBundle::default(), opts);
        }

        let bundle = super::source::load_from_env().await.unwrap_or_else(|e| {
            warn!(error = %e, "failed to load policy bundle; starting with pass-through (no policies)");
            PolicyBundle::default()
        });

        Self::from_bundle(&bundle, opts)
    }

    fn compile(bundle: &PolicyBundle, enabled: bool) -> EngineState {
        let mut text_policies = Vec::new();
        let mut cost_caps = Vec::new();
        let mut rate_limits = Vec::new();

        // Sort by priority (lower first) then by name for determinism.
        let mut policies: Vec<&Policy> = bundle.policies.iter().filter(|p| p.is_active()).collect();
        policies.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));

        for p in policies {
            match p.policy_type {
                PolicyType::CostCap => {
                    match serde_json::from_value::<CostCapConfig>(p.config.clone()) {
                        Ok(config) => cost_caps.push(CostCapPolicy {
                            meta: PolicyMeta::from_policy(p),
                            config,
                        }),
                        Err(e) => {
                            warn!(policy = %p.id(), error = %e, "invalid cost_cap config; skipping")
                        }
                    }
                }
                PolicyType::RateLimit => {
                    match serde_json::from_value::<RateLimitConfig>(p.config.clone()) {
                        Ok(config) => rate_limits.push(RateLimitPolicy {
                            meta: PolicyMeta::from_policy(p),
                            config,
                        }),
                        Err(e) => {
                            warn!(policy = %p.id(), error = %e, "invalid rate_limit config; skipping")
                        }
                    }
                }
                _ => {
                    if let Some(rule) = compile_rule(p) {
                        text_policies.push(CompiledPolicy {
                            meta: PolicyMeta::from_policy(p),
                            rule,
                        });
                    }
                }
            }
        }

        info!(
            text_policies = text_policies.len(),
            cost_caps = cost_caps.len(),
            rate_limits = rate_limits.len(),
            "compiled Nova Guard policy set"
        );

        EngineState {
            enabled,
            text_policies,
            cost_caps,
            rate_limits,
        }
    }

    /// Replace the active policy set atomically (used by the control-plane refresh).
    pub fn swap_bundle(&self, bundle: &PolicyBundle) {
        let enabled = self.state.load().enabled;
        self.state.store(Arc::new(Self::compile(bundle, enabled)));
    }

    pub fn is_enabled(&self) -> bool {
        self.state.load().enabled
    }

    pub fn active_policy_count(&self) -> usize {
        self.state.load().active_count()
    }

    pub fn block_mode(&self) -> BlockResponseMode {
        self.block_mode
    }

    /// Build the final [`PolicyDecision`] from a rule outcome and metadata,
    /// applying mode semantics.
    fn decision_from_outcome(
        meta: &PolicyMeta,
        outcome: RuleOutcome,
        latency_ms: f64,
    ) -> PolicyDecision {
        // In shadow mode the action is recorded but never applied.
        let effective_action = outcome.action;
        PolicyDecision {
            policy_id: meta.id.clone(),
            policy_name: meta.name.clone(),
            policy_type: meta.policy_type.as_str().to_string(),
            mode: meta.mode,
            score: outcome.score,
            severity: Severity::from_score(outcome.score),
            flagged: outcome.flagged,
            action: if outcome.flagged {
                effective_action
            } else {
                PolicyAction::Allow
            },
            transformed_text: outcome.transformed_text,
            reason: outcome.reason,
            matched_entities: outcome.matched_entities,
            latency_ms,
        }
    }

    /// Evaluate all applicable policies for a phase.
    ///
    /// `model` is the request model id (lowercased by the caller is fine but not
    /// required — rules lowercase as needed). `text` is the flattened payload
    /// text. `json` is the full request/response body. `live_state` carries
    /// cost/rate counters when available.
    pub fn evaluate(
        &self,
        phase: Phase,
        model: &str,
        text: &str,
        json: Option<&serde_json::Value>,
        input_tokens: Option<u32>,
        live_state: Option<&LiveState>,
    ) -> EvaluationResult {
        let state = self.state.load();
        let mut result = EvaluationResult::default();
        if !state.enabled {
            return result;
        }

        let model_lc = model.to_lowercase();
        let mut working_text: Cow<str> = Cow::Borrowed(text);

        // 1) Cost-cap + rate-limit policies run first (input phase only); they can
        //    block before any text scanning.
        if phase.applies_to(Phase::Input) {
            for cc in &state.cost_caps {
                let decision = self.eval_cost_cap(cc, &model_lc, input_tokens, live_state);
                if decision.is_blocking() && result.block.is_none() {
                    result.block = Some(decision.clone());
                }
                result.decisions.push(decision);
                if result.block.is_some() {
                    return result; // block is terminal
                }
            }
            for rl in &state.rate_limits {
                let decision = self.eval_rate_limit(rl, live_state);
                if decision.is_blocking() && result.block.is_none() {
                    result.block = Some(decision.clone());
                }
                result.decisions.push(decision);
                if result.block.is_some() {
                    return result;
                }
            }
        }

        // 2) Text rules, in priority order, composing transforms.
        for cp in &state.text_policies {
            if !cp.rule.phase().applies_to(phase) {
                continue;
            }
            let ctx = EvalContext {
                phase,
                model: &model_lc,
                text: Cow::Borrowed(working_text.as_ref()),
                json,
                input_tokens,
                live_state,
            };
            let start = Instant::now();
            let outcome = cp.rule.evaluate(&ctx);
            let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
            let decision = Self::decision_from_outcome(&cp.meta, outcome, latency_ms);

            // Apply transform if enforced.
            if decision.is_applied_transform() {
                if let Some(t) = &decision.transformed_text {
                    working_text = Cow::Owned(t.clone());
                    result.transformed_text = Some(t.clone());
                }
            }

            let blocking = decision.is_blocking();
            result.decisions.push(decision.clone());
            if blocking {
                result.block = Some(decision);
                return result; // terminal
            }
        }

        result
    }

    fn eval_cost_cap(
        &self,
        cc: &CostCapPolicy,
        model: &str,
        _input_tokens: Option<u32>,
        live_state: Option<&LiveState>,
    ) -> PolicyDecision {
        // Out-of-scope models pass.
        if let Some(scope) = &cc.config.scope_to_models {
            if !scope.is_empty() && !scope.iter().any(|m| m.to_lowercase() == model) {
                return PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
            }
        }

        let window_key = window_label(cc.config.window);
        let spend = live_state.and_then(|s| s.cost_usd_by_window.get(window_key).copied());

        match spend {
            Some(spent) => {
                let over_hard = spent >= cc.config.max_usd;
                let over_soft = cc
                    .config
                    .soft_usd
                    .map(|soft| spent >= soft)
                    .unwrap_or(false);
                if over_hard {
                    let mut d =
                        PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
                    d.flagged = true;
                    d.score = 1.0;
                    d.severity = Severity::Critical;
                    d.action = cc.config.action;
                    d.reason = format!(
                        "spend ${spent:.2} over {window_key} reached cap ${:.2}",
                        cc.config.max_usd
                    );
                    d
                } else if over_soft {
                    let mut d =
                        PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
                    d.flagged = true;
                    d.score = 0.6;
                    d.severity = Severity::High;
                    d.action = PolicyAction::FlagOnly; // soft cap warns, never blocks
                    d.reason = format!(
                        "spend ${spent:.2} over {window_key} crossed soft cap ${:.2}",
                        cc.config.soft_usd.unwrap_or_default()
                    );
                    d
                } else {
                    PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode)
                }
            }
            None => self.unavailable_state_decision(
                &cc.meta,
                "cost_cap",
                cc.config.action,
                cc.config.enforcement_mode == CostEnforcementMode::Strict,
            ),
        }
    }

    fn eval_rate_limit(
        &self,
        rl: &RateLimitPolicy,
        live_state: Option<&LiveState>,
    ) -> PolicyDecision {
        match live_state {
            Some(state) => {
                for w in &rl.config.windows {
                    if let Some(max) = w.max_requests {
                        if let Some(&count) = state.requests_by_window.get(&w.period) {
                            if count >= max {
                                let mut d = PolicyDecision::allow(
                                    &rl.meta.id,
                                    &rl.meta.name,
                                    "rate_limit",
                                    rl.meta.mode,
                                );
                                d.flagged = true;
                                d.score = 1.0;
                                d.severity = Severity::High;
                                d.action = w.action;
                                d.reason = format!(
                                    "requests {count} reached limit {max} per {}",
                                    w.period
                                );
                                return d;
                            }
                        }
                    }
                    if let Some(max) = w.max_tokens {
                        if let Some(&count) = state.tokens_by_window.get(&w.period) {
                            if count >= max {
                                let mut d = PolicyDecision::allow(
                                    &rl.meta.id,
                                    &rl.meta.name,
                                    "rate_limit",
                                    rl.meta.mode,
                                );
                                d.flagged = true;
                                d.score = 1.0;
                                d.severity = Severity::High;
                                d.action = w.action;
                                d.reason =
                                    format!("tokens {count} reached limit {max} per {}", w.period);
                                return d;
                            }
                        }
                    }
                }
                PolicyDecision::allow(&rl.meta.id, &rl.meta.name, "rate_limit", rl.meta.mode)
            }
            None => {
                self.unavailable_state_decision(&rl.meta, "rate_limit", PolicyAction::Block, false)
            }
        }
    }

    /// Decision when live state is unavailable: fail-closed policies block;
    /// fail-open policies (the default) allow.
    fn unavailable_state_decision(
        &self,
        meta: &PolicyMeta,
        policy_type: &str,
        action: PolicyAction,
        strict: bool,
    ) -> PolicyDecision {
        let fail_closed = meta.fail_closed || (strict && !self.fail_open_default);
        if fail_closed {
            let mut d = PolicyDecision::allow(&meta.id, &meta.name, policy_type, meta.mode);
            d.flagged = true;
            d.score = 1.0;
            d.severity = Severity::Critical;
            d.action = action;
            d.reason = "live cost/rate state unavailable; failing closed".to_string();
            d
        } else {
            let mut d = PolicyDecision::allow(&meta.id, &meta.name, policy_type, meta.mode);
            d.reason = "live cost/rate state unavailable; failing open".to_string();
            d
        }
    }
}

/// Map a cost window to the live-state key used by the control plane.
fn window_label(w: CostWindow) -> &'static str {
    match w {
        CostWindow::OneDayRolling => "1d_rolling",
        CostWindow::SevenDayRolling => "7d_rolling",
        CostWindow::ThirtyDayRolling => "30d_rolling",
        CostWindow::OneDayCalendar => "1d_calendar",
        CostWindow::OneMonthCalendar => "1mo_calendar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn engine(json: &str) -> PolicyEngine {
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        PolicyEngine::from_bundle(&bundle, EngineOptions::default())
    }

    #[test]
    fn disabled_engine_allows_everything() {
        let e = PolicyEngine::disabled();
        assert!(!e.is_enabled());
        let r = e.evaluate(Phase::Input, "gpt-4o", "ssn 123-45-6789", None, None, None);
        assert!(r.decisions.is_empty());
        assert!(!r.is_blocked());
    }

    #[test]
    fn regex_block_on_input() {
        let e = engine(
            r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"enforce",
            "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
        );
        let r = e.evaluate(
            Phase::Input,
            "gpt-4o",
            "my ssn 123-45-6789",
            None,
            None,
            None,
        );
        assert!(r.is_blocked());
        assert_eq!(r.block.unwrap().policy_type, "regex_match");
    }

    #[test]
    fn shadow_mode_records_but_does_not_block() {
        let e = engine(
            r#"{"policies":[{"name":"ssn","type":"regex_match","mode":"shadow",
            "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}}]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "123-45-6789", None, None, None);
        assert!(!r.is_blocked());
        assert_eq!(r.decisions.len(), 1);
        assert!(r.decisions[0].flagged);
    }

    #[test]
    fn transform_composition_threads_text() {
        // pii mask then banned check sees masked text. Use two policies.
        let e = engine(
            r#"{"policies":[
              {"name":"pii","type":"pii_detection","mode":"enforce","priority":10,
               "config":{"phase":"input","entities":["EMAIL_ADDRESS"],"action":"redact"}},
              {"name":"flagmail","type":"regex_match","mode":"enforce","priority":20,
               "config":{"phase":"input","patterns":[{"name":"mail","regex":"@"}],"action":"flag_only"}}
            ]}"#,
        );
        let r = e.evaluate(
            Phase::Input,
            "gpt-4o",
            "write to a@b.com now",
            None,
            None,
            None,
        );
        assert!(r.transformed_text.is_some());
        assert!(!r.transformed_text.as_ref().unwrap().contains("a@b.com"));
    }

    #[test]
    fn priority_ordering_first_block_wins() {
        let e = engine(
            r#"{"policies":[
              {"name":"low","type":"regex_match","mode":"enforce","priority":200,
               "config":{"phase":"input","patterns":[{"name":"a","regex":"alpha"}],"action":"block"}},
              {"name":"high","type":"regex_match","mode":"enforce","priority":10,
               "config":{"phase":"input","patterns":[{"name":"b","regex":"alpha"}],"action":"block"}}
            ]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "alpha", None, None, None);
        // higher priority (lower number) runs first and blocks
        assert_eq!(r.block.unwrap().policy_name, "high");
    }

    #[test]
    fn cost_cap_blocks_when_over_hard_cap() {
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 150.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(r.is_blocked());
    }

    #[test]
    fn cost_cap_allows_under_cap() {
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 50.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(!r.is_blocked());
    }

    #[test]
    fn cost_cap_soft_cap_flags_only() {
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"softUsd":80.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 90.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(!r.is_blocked());
        assert!(r.decisions[0].flagged);
        assert_eq!(r.decisions[0].action, PolicyAction::FlagOnly);
    }

    #[test]
    fn cost_cap_fail_open_when_state_missing() {
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, None);
        assert!(
            !r.is_blocked(),
            "advisory cost_cap fails open without state"
        );
    }

    #[test]
    fn cost_cap_fail_closed_when_marked() {
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, None);
        assert!(r.is_blocked(), "fail-closed cost_cap blocks without state");
    }

    #[test]
    fn rate_limit_blocks_over_request_cap() {
        let e = engine(
            r#"{"policies":[{"name":"rl","type":"rate_limit","mode":"enforce",
            "config":{"windows":[{"period":"1m","maxRequests":60,"action":"block"}]}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.requests_by_window.insert("1m".into(), 100);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(r.is_blocked());
    }

    #[test]
    fn output_phase_policy_not_run_on_input() {
        let e = engine(
            r#"{"policies":[{"name":"out","type":"regex_match","mode":"enforce",
            "config":{"phase":"output","patterns":[{"name":"x","regex":"secret"}],"action":"block"}}]}"#,
        );
        // input phase: output-only policy must not run
        let ri = e.evaluate(Phase::Input, "gpt-4o", "this is secret", None, None, None);
        assert!(!ri.is_blocked());
        // output phase: it runs and blocks
        let ro = e.evaluate(Phase::Output, "gpt-4o", "this is secret", None, None, None);
        assert!(ro.is_blocked());
    }

    #[test]
    fn active_policy_count_excludes_inactive() {
        let e = engine(
            r#"{"policies":[
              {"name":"on","type":"model_allowlist","mode":"enforce","config":{"allowed":["gpt-4o"]}},
              {"name":"off","type":"model_allowlist","mode":"off","config":{"allowed":["gpt-4o"]}}
            ]}"#,
        );
        assert_eq!(e.active_policy_count(), 1);
    }

    #[test]
    fn swap_bundle_updates_policies() {
        let e = engine(r#"{"policies":[]}"#);
        assert_eq!(e.active_policy_count(), 0);
        let b = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"m","type":"model_allowlist","mode":"enforce","config":{"allowed":["gpt-4o"]}}]}"#,
        )
        .unwrap();
        e.swap_bundle(&b);
        assert_eq!(e.active_policy_count(), 1);
    }

    #[test]
    fn block_is_terminal_subsequent_not_evaluated() {
        let e = engine(
            r#"{"policies":[
              {"name":"first","type":"regex_match","mode":"enforce","priority":10,
               "config":{"phase":"input","patterns":[{"name":"a","regex":"boom"}],"action":"block"}},
              {"name":"second","type":"regex_match","mode":"enforce","priority":20,
               "config":{"phase":"input","patterns":[{"name":"b","regex":"boom"}],"action":"flag_only"}}
            ]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "boom", None, None, None);
        assert!(r.is_blocked());
        // only the first decision recorded; second not evaluated
        assert_eq!(r.decisions.len(), 1);
    }

    #[test]
    fn empty_live_state_struct_constructs() {
        let ls = LiveState {
            cost_usd_by_window: HashMap::new(),
            requests_by_window: HashMap::new(),
            tokens_by_window: HashMap::new(),
        };
        assert!(ls.cost_usd_by_window.is_empty());
    }
}
