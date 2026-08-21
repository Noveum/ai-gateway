//! The Nova Guard policy engine.
//!
//! The engine compiles a [`PolicyBundle`] into runnable rules, orders them by
//! priority, and evaluates them per phase. It applies mode semantics
//! (off/shadow/enforce), composes transforms (each policy sees the previous
//! policy's mutated text), and short-circuits on the first enforced block.
//!
//! State is held behind an [`arc_swap::ArcSwap`] so the active policy set can be
//! hot-swapped at runtime ([`PolicyEngine::swap_bundle`]) without locking the
//! request hot path.

use std::borrow::Cow;
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use arc_swap::ArcSwap;
use tracing::{info, warn};

use super::config::{
    resolve_strict, validate_policy, CostCapConfig, CostEnforcementMode, CostWindow, Policy,
    PolicyBundle, PolicyRejection, PolicyType, RateLimitConfig,
};
use super::decision::{Phase, PolicyAction, PolicyDecision, PolicyMode, Severity};
use super::rules::{compile_rule, EvalContext, LiveState, PolicyRule, RuleOutcome};
use super::synthetic::BlockResponseMode;
use tracing::error;

/// Per-policy metadata carried alongside the compiled rule.
struct PolicyMeta {
    id: String,
    name: String,
    policy_type: PolicyType,
    mode: PolicyMode,
    fail_closed: bool,
    /// Org-sourced (from the platform's merged `/effective` set): enforce
    /// against org-scope counters when the control plane provides them.
    org_scoped: bool,
}

impl PolicyMeta {
    fn from_policy(p: &Policy) -> Self {
        let org_scoped = p.source.as_deref().is_some_and(|s| {
            s.eq_ignore_ascii_case("org") || s.eq_ignore_ascii_case("organization")
        });
        Self {
            id: p.id().to_string(),
            name: p.name.clone(),
            policy_type: p.kind(),
            mode: p.mode,
            fail_closed: p.fail_closed,
            org_scoped,
        }
    }
}

/// An active policy the engine could not compile.
///
/// Recorded rather than dropped. A policy an operator created in the UI that
/// does nothing is the failure mode this exists to prevent: every rejection is
/// logged at `error!`, counted, and readable via
/// [`PolicyEngine::rejected_policies`]. When the policy is `failClosed` it also
/// blocks traffic — the operator asked for "block when this cannot be
/// evaluated", and a policy that will never compile can never be evaluated.
struct RejectedPolicy {
    id: String,
    name: String,
    /// The raw wire `type`, which for an unknown type is the whole point.
    type_name: String,
    mode: PolicyMode,
    fail_closed: bool,
    rejection: PolicyRejection,
}

impl RejectedPolicy {
    /// The operator-facing one-liner, reused by the log line, the accessor and
    /// the block reason so they can never describe the same fault differently.
    fn message(&self) -> String {
        format!(
            "policy '{}' (type '{}') was rejected: {}",
            self.name, self.type_name, self.rejection
        )
    }

    /// Does this rejection block traffic? Only when the operator marked the
    /// policy `failClosed` **and** did not put it in shadow mode. Shadow means
    /// "record, never apply", and that has to hold for a compile failure too or
    /// a shadow rollout could take production down.
    fn blocks(&self) -> bool {
        self.fail_closed && self.mode == PolicyMode::Enforce
    }
}

/// Format a USD amount for a human-readable policy reason. Uses 2 decimals for
/// normal amounts, but keeps up to 6 (trimmed) for sub-cent caps so a tiny cap
/// like `$0.00001` isn't rounded to a meaningless `$0.00` in the audit log.
fn fmt_usd(v: f64) -> String {
    // Anything that rounds to zero at 6 decimals (incl. 0.0 and -0.0) → "0.00".
    if v.abs() < 0.000_000_5 {
        "0.00".to_string()
    } else if v.abs() < 0.01 {
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        format!("{v:.2}")
    }
}

/// `cost_cap` / `rate_limit` need a cross-request live-state backend to evaluate
/// spend/rate. None is currently bundled, so they always fail open. A policy
/// authored with `failClosed: true` would otherwise block 100% of traffic
/// permanently (state can never arrive), so we neutralize that flag at compile
/// time with a warning rather than ship a self-inflicted outage.
fn neutralize_stateful_fail_closed(mut meta: PolicyMeta) -> PolicyMeta {
    if meta.fail_closed {
        warn!(
            policy = %meta.id, kind = ?meta.policy_type,
            "cost_cap/rate_limit `failClosed` has no live-state backend to enforce against; \
             forcing fail-open to avoid blocking all traffic"
        );
        meta.fail_closed = false;
    }
    meta
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

/// One canonical scope predicate for both predictive evaluation and the
/// strict-mode requirement for a provider-enforced output bound.
fn cost_cap_applies_to_model(cc: &CostCapPolicy, model: &str) -> bool {
    cc.config.scope_to_models.as_ref().is_none_or(|scope| {
        scope.is_empty()
            || scope
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(model))
    })
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
    rejected: Vec<RejectedPolicy>,
}

impl EngineState {
    fn active_count(&self) -> usize {
        // A fail-closed rejection blocks every request, so it counts as active.
        // Callers gate the entire guard path on this being non-zero
        // (`middleware.rs`, `worker_rt.rs`); leaving it out would mean a policy
        // whose whole purpose is to block traffic gets skipped by the very check
        // that decides whether to run the guard at all.
        self.text_policies.len()
            + self.cost_caps.len()
            + self.rate_limits.len()
            + self.rejected.iter().filter(|r| r.blocks()).count()
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
    /// Whether a live cost/rate state backend (the platform bridge) is wired in.
    /// When `false`, `cost_cap`/`rate_limit` `failClosed` is neutralized at compile
    /// time — no state can ever arrive, so honoring it would block all traffic
    /// permanently. When `true`, `failClosed` is honored (a transient `/state`
    /// outage blocks, as intended).
    pub live_state_backed: bool,
}

impl EngineOptions {
    /// Build options from the standard env vars (`NOVEUM_GUARD_ENABLED`,
    /// `NOVEUM_GUARD_BLOCK_RESPONSE_MODE`). Shared by the local and the
    /// platform-fetched policy paths so they enforce identically.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_env() -> Self {
        let enabled = std::env::var("NOVEUM_GUARD_ENABLED")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "false" | "0" | "no" | "off" | "disabled" | ""
                )
            })
            .unwrap_or(true);
        let block_mode = std::env::var("NOVEUM_GUARD_BLOCK_RESPONSE_MODE")
            .map(|v| BlockResponseMode::from_env_str(&v))
            .unwrap_or(BlockResponseMode::SyntheticSuccess);
        Self {
            enabled,
            block_mode,
            fail_open_default: true,
            live_state_backed: false,
        }
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            block_mode: BlockResponseMode::SyntheticSuccess,
            fail_open_default: true,
            live_state_backed: false,
        }
    }
}

pub struct PolicyEngine {
    state: ArcSwap<EngineState>,
    block_mode: BlockResponseMode,
    fail_open_default: bool,
    live_state_backed: bool,
}

impl PolicyEngine {
    /// Build an engine from a bundle and options.
    pub fn from_bundle(bundle: &PolicyBundle, opts: EngineOptions) -> Self {
        let state = Self::compile(bundle, opts.enabled, opts.live_state_backed);
        Self {
            state: ArcSwap::from_pointee(state),
            block_mode: opts.block_mode,
            fail_open_default: opts.fail_open_default,
            live_state_backed: opts.live_state_backed,
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
    /// An *absent* policy source stays pass-through (that's the default
    /// deployment). A *configured but unreadable or malformed* one is an error:
    /// degrading it to pass-through leaves the operator believing policies they
    /// wrote are in force. The caller refuses to start.
    ///
    /// Native only (reads env + filesystem). The Cloudflare Worker builds the
    /// engine from an in-memory bundle via [`PolicyEngine::from_bundle`].
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn from_env() -> Result<Self, String> {
        // Single source of truth for the env parsing (falsey-value list,
        // block-mode mapping) — shared with the platform path via
        // `EngineOptions::from_env` so the three call sites can't drift.
        let opts = EngineOptions::from_env();

        if !opts.enabled {
            return Ok(Self::from_bundle(&PolicyBundle::default(), opts));
        }

        // `load_from_env` already separates the two cases: it returns an empty
        // bundle when no source is configured, and an error only when one is
        // configured and cannot be loaded.
        let bundle = super::source::load_from_env().await?;
        bundle.schema_version_supported()?;

        let engine = Self::from_bundle(&bundle, opts);
        // A locally configured bundle is authored by the operator running this
        // process, so an unenforceable policy in it is a deploy-time mistake we
        // can still refuse. Starting anyway would ship a gateway whose policy
        // file says one thing and whose behavior says another.
        let rejected = engine.rejected_policies();
        if !rejected.is_empty() {
            return Err(format!(
                "Nova Guard policy bundle contains {} policy/policies this build cannot enforce:\n  - {}",
                rejected.len(),
                rejected.join("\n  - ")
            ));
        }

        Ok(engine)
    }

    fn compile(bundle: &PolicyBundle, enabled: bool, live_state_backed: bool) -> EngineState {
        let mut text_policies = Vec::new();
        let mut cost_caps = Vec::new();
        let mut rate_limits = Vec::new();
        let mut rejected: Vec<RejectedPolicy> = Vec::new();

        // Only neutralize `failClosed` when there's no state backend to enforce
        // against; with the platform bridge wired, `failClosed` is honored.
        let stateful_meta = |p: &Policy| {
            let meta = PolicyMeta::from_policy(p);
            if meta.org_scoped {
                info!(
                    policy = %meta.id,
                    "org-sourced policy: enforcing against org counters only; a /state response without them is treated as unavailable state, never as project counters"
                );
            }
            if live_state_backed {
                meta
            } else {
                neutralize_stateful_fail_closed(meta)
            }
        };

        // Sort by priority (lower first) then by name for determinism.
        let mut policies: Vec<&Policy> = bundle.policies.iter().filter(|p| p.is_active()).collect();
        policies.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));

        // A policy that cannot be compiled is recorded, never dropped. Silently
        // skipping one leaves the operator believing a guardrail they created in
        // the UI is protecting them when it is inert.
        let mut reject = |p: &Policy, rejection: PolicyRejection| {
            let r = RejectedPolicy {
                id: p.id().to_string(),
                name: p.name.clone(),
                type_name: p.type_name().to_string(),
                mode: p.mode,
                fail_closed: p.fail_closed,
                rejection,
            };
            if r.blocks() {
                error!(policy = %r.id, "{} — failClosed, so it blocks all traffic until fixed", r.message());
            } else {
                error!(policy = %r.id, "{} — this policy is NOT in force", r.message());
            }
            rejected.push(r);
        };

        for p in policies {
            // Schema first: the contract decides whether a type exists and
            // whether its config is well formed, so every deployment target and
            // every language agrees on the answer.
            if let Err(rejection) = validate_policy(p) {
                reject(p, rejection);
                continue;
            }
            match p.kind() {
                PolicyType::CostCap => {
                    match serde_json::from_value::<CostCapConfig>(p.config.clone()) {
                        Ok(config) => cost_caps.push(CostCapPolicy {
                            meta: stateful_meta(p),
                            config,
                        }),
                        // The schema accepted this config, so a parse failure
                        // here means the Rust struct and the schema disagree —
                        // a contract bug, not bad operator input.
                        Err(e) => reject(
                            p,
                            PolicyRejection::InvalidConfig {
                                errors: vec![format!(
                                    "config satisfies the schema but not CostCapConfig: {e}"
                                )],
                            },
                        ),
                    }
                }
                PolicyType::RateLimit => {
                    match serde_json::from_value::<RateLimitConfig>(p.config.clone()) {
                        Ok(config) => rate_limits.push(RateLimitPolicy {
                            meta: stateful_meta(p),
                            config,
                        }),
                        Err(e) => reject(
                            p,
                            PolicyRejection::InvalidConfig {
                                errors: vec![format!(
                                    "config satisfies the schema but not RateLimitConfig: {e}"
                                )],
                            },
                        ),
                    }
                }
                _ => match compile_rule(p) {
                    Some(rule) => text_policies.push(CompiledPolicy {
                        meta: PolicyMeta::from_policy(p),
                        rule,
                    }),
                    None => reject(
                        p,
                        PolicyRejection::InvalidConfig {
                            errors: vec![
                                "the rule layer refused this config (see the preceding log line)"
                                    .to_string(),
                            ],
                        },
                    ),
                },
            }
        }

        if rejected.is_empty() {
            info!(
                text_policies = text_policies.len(),
                cost_caps = cost_caps.len(),
                rate_limits = rate_limits.len(),
                "compiled Nova Guard policy set"
            );
        } else {
            error!(
                text_policies = text_policies.len(),
                cost_caps = cost_caps.len(),
                rate_limits = rate_limits.len(),
                rejected = rejected.len(),
                blocking = rejected.iter().filter(|r| r.blocks()).count(),
                "compiled Nova Guard policy set WITH REJECTIONS; the rejected policies are not enforcing anything"
            );
        }

        EngineState {
            enabled,
            text_policies,
            cost_caps,
            rate_limits,
            rejected,
        }
    }

    /// Replace the active policy set atomically at runtime (lock-free via
    /// `ArcSwap`). Lets an embedder hot-reload policies without restarting; the
    /// enabled/disabled flag is preserved.
    pub fn swap_bundle(&self, bundle: &PolicyBundle) {
        let enabled = self.state.load().enabled;
        self.state.store(Arc::new(Self::compile(
            bundle,
            enabled,
            self.live_state_backed,
        )));
    }

    pub fn is_enabled(&self) -> bool {
        self.state.load().enabled
    }

    pub fn active_policy_count(&self) -> usize {
        self.state.load().active_count()
    }

    /// Active policies the engine refused to compile (unknown type, config that
    /// violates the schema, or a type this build cannot enforce).
    ///
    /// Non-empty means an operator authored a guardrail that is doing nothing.
    /// The bootstrap path turns this into a startup failure; the hot-reload path
    /// cannot refuse to start, so it surfaces here and in the `error!` log.
    pub fn rejected_policies(&self) -> Vec<String> {
        self.state
            .load()
            .rejected
            .iter()
            .map(|r| r.message())
            .collect()
    }

    /// How many active policies were rejected.
    pub fn rejected_policy_count(&self) -> usize {
        self.state.load().rejected.len()
    }

    /// Number of active policies that can only be evaluated against a live
    /// cross-request state backend (`cost_cap` + `rate_limit`). Deployment
    /// targets without such a backend — notably the Cloudflare Worker — use this
    /// to refuse loudly instead of admitting traffic past a limit that is
    /// silently a no-op.
    pub fn stateful_policy_count(&self) -> usize {
        let s = self.state.load();
        s.cost_caps.len() + s.rate_limits.len()
    }

    /// Does the active policy set contain a `cost_cap` in **strict** enforcement
    /// mode? Strict means the cap must hold across replicas, so the middleware
    /// routes the request through the platform's atomic admission API instead of
    /// the per-process pending ledger (see [`crate::policy::admission`]).
    ///
    /// Re-read per request because the policy set hot-swaps; `O(active caps)`
    /// over an `ArcSwap` load, no locking.
    pub fn has_strict_cost_cap(&self) -> bool {
        self.state
            .load()
            .cost_caps
            .iter()
            .any(|cc| cc.config.enforcement_mode == CostEnforcementMode::Strict)
    }

    /// Does this model need an explicit provider-enforced completion budget for
    /// a cost cap that the supplied deployment override routes through atomic
    /// admission?
    ///
    /// This is deliberately narrower than "does admission run": the Worker
    /// also routes rate limits through the platform, and a rate-only policy
    /// must not acquire the strict cost-cap requirement for an explicit output
    /// budget.  Conversely, a deployment-wide `strict` override routes an
    /// otherwise advisory cap through admission and must receive the same hard
    /// cap semantics as a self-declared strict cap.
    pub fn requires_explicit_output_limit(
        &self,
        model: &str,
        override_mode: Option<CostEnforcementMode>,
    ) -> bool {
        let model = model.to_ascii_lowercase();
        self.state.load().cost_caps.iter().any(|cc| {
            resolve_strict(
                override_mode,
                cc.config.enforcement_mode == CostEnforcementMode::Strict,
            ) && cc.meta.mode == PolicyMode::Enforce
                && cc.config.action.is_block()
                && cost_cap_applies_to_model(cc, &model)
        })
    }

    /// Does an enforcing/blocking strict cap require a JSON request whose
    /// provider-side input can be bounded before admission?
    ///
    /// Unlike [`Self::requires_explicit_output_limit`], this cannot filter by
    /// model scope: a multipart/binary request has no trustworthy parsed model
    /// with which to prove it is out of scope. Rejecting that opaque shape is
    /// safer than silently bypassing a cap the operator configured as strict.
    pub fn requires_bounded_json_input(&self, override_mode: Option<CostEnforcementMode>) -> bool {
        self.state.load().cost_caps.iter().any(|cc| {
            resolve_strict(
                override_mode,
                cc.config.enforcement_mode == CostEnforcementMode::Strict,
            ) && cc.meta.mode == PolicyMode::Enforce
                && cc.config.action.is_block()
        })
    }

    /// The decision to apply when platform **admission** could not be evaluated.
    ///
    /// Considers **exactly the checks the platform put in this admission
    /// request**. By contract, the platform always evaluates every active
    /// `rate_limit`, but evaluates a `cost_cap` only when either the policy is
    /// strict or the native deployment explicitly sent
    /// `forceStrictCostCaps: true`. Model-scoped cost caps are considered only
    /// for the same model the platform received.
    ///
    /// Per considered cap the fail-closed/fail-open computation is the one an
    /// unavailable `/state` uses
    /// ([`PolicyEngine::unavailable_state_decision`]): a `failClosed` policy
    /// blocks, everything else allows with the explicit reason recorded.
    ///
    /// Returns the blocking decision if any considered policy fails closed,
    /// else the first fail-open decision (whose `reason` names the outage),
    /// else `None` when the platform had no applicable stateful check.
    pub fn admission_unavailable_decision(
        &self,
        reason: &str,
        model: &str,
        force_strict_cost_caps: bool,
    ) -> Option<PolicyDecision> {
        let state = self.state.load();
        let mut fail_open: Option<PolicyDecision> = None;
        for cc in state.cost_caps.iter().filter(|cc| {
            (force_strict_cost_caps || cc.config.enforcement_mode == CostEnforcementMode::Strict)
                && cost_cap_applies_to_model(cc, model)
        }) {
            let d = self.unavailable_decision_with_reason(
                &cc.meta,
                "cost_cap",
                cc.config.action,
                true,
                &format!("platform admission unavailable ({reason})"),
            );
            if d.is_blocking() {
                return Some(d);
            }
            fail_open.get_or_insert(d);
        }

        for rl in &state.rate_limits {
            // The platform admission schema constrains rate-limit actions to
            // BLOCK. Keep the configured action when present so the gateway
            // remains honest if that contract ever broadens; an empty window
            // list is rejected upstream, and BLOCK is the safe fallback.
            let action = rl
                .config
                .windows
                .first()
                .map_or(PolicyAction::Block, |window| window.action);
            let d = self.unavailable_decision_with_reason(
                &rl.meta,
                "rate_limit",
                action,
                false,
                &format!("platform admission unavailable ({reason})"),
            );
            if d.is_blocking() {
                return Some(d);
            }
            fail_open.get_or_insert(d);
        }
        fail_open
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
        // `Phase::Both` is a *policy* attribute, not an evaluation axis. The
        // middleware must call this with `Input` or `Output` only; evaluating
        // with `Both` would run input- and output-only policies together and
        // double-apply `Both` transforms. Guard at runtime (not just debug) so an
        // out-of-tree caller can't silently evaluate under an invalid axis.
        let result = EvaluationResult::default();
        if phase == Phase::Both {
            debug_assert!(
                false,
                "evaluate() must be called with Phase::Input or Phase::Output, not Both"
            );
            warn!("Nova Guard: evaluate() called with Phase::Both; returning no-op result");
            return result;
        }
        let state = self.state.load();
        let mut result = result;
        if !state.enabled {
            return result;
        }

        let model_lc = model.to_lowercase();
        let mut working_text: Cow<str> = Cow::Borrowed(text);

        // The request's own completion budget, for predictive cost-cap admission.
        let max_output_tokens = json.and_then(|j| {
            ["max_tokens", "max_completion_tokens", "max_output_tokens"]
                .iter()
                .find_map(|k| j.get(*k).and_then(|v| v.as_u64()))
        });

        // 0) A `failClosed` policy the engine could not compile blocks, at the
        //    input phase, before anything else. `failClosed` means "block when
        //    this policy cannot be evaluated", and a policy that will never
        //    compile can never be evaluated. Failing open here would deliver the
        //    exact outcome the operator wrote `failClosed` to prevent.
        if phase.applies_to(Phase::Input) {
            // The first blocking rejection is terminal, exactly like the first
            // enforced block from a compiled policy.
            if let Some(r) = state.rejected.iter().find(|r| r.blocks()) {
                let mut d = PolicyDecision::allow(
                    &r.id,
                    &r.name,
                    r.type_name.as_str(),
                    PolicyMode::Enforce,
                );
                d.flagged = true;
                d.score = 1.0;
                d.severity = Severity::Critical;
                d.action = PolicyAction::Block;
                d.reason = format!("{}; failing closed", r.message());
                result.block = Some(d.clone());
                result.decisions.push(d);
                return result;
            }
        }

        // 1) Cost-cap + rate-limit policies run first (input phase only); they can
        //    block before any text scanning.
        if phase.applies_to(Phase::Input) {
            for cc in &state.cost_caps {
                let decision =
                    self.eval_cost_cap(cc, &model_lc, input_tokens, max_output_tokens, live_state);
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
            // `std::time::Instant` is unsupported on wasm32 (Cloudflare Worker)
            // and panics; per-policy latency is telemetry only, so skip it there.
            #[cfg(not(target_arch = "wasm32"))]
            let start = Instant::now();
            let outcome = cp.rule.evaluate(&ctx);
            #[cfg(not(target_arch = "wasm32"))]
            let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
            #[cfg(target_arch = "wasm32")]
            let latency_ms = 0.0_f64;
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

    /// Apply only enforce-mode *transform* actions (redact/mask/hash/replace)
    /// from text rules matching `phase` to a single text segment, composing in
    /// priority order. Returns `Some(new_text)` when a transform changed the
    /// text, else `None`.
    ///
    /// This deliberately does NOT evaluate cost/rate/token/block policies — the
    /// aggregate block decision is made once by [`Self::evaluate`] over the full
    /// flattened payload; this method only rewrites individual structured
    /// segments (chat message contents) so the forwarded body stays valid and
    /// blocking is never re-litigated per segment.
    pub fn apply_text_transforms(&self, phase: Phase, model: &str, text: &str) -> Option<String> {
        // `Phase::Both` is a policy attribute, not an evaluation axis (see
        // `evaluate`). Guard at runtime so a release build can't transform under
        // an invalid axis.
        if phase == Phase::Both {
            debug_assert!(false, "apply_text_transforms requires Input or Output");
            warn!("Nova Guard: apply_text_transforms called with Phase::Both; skipping");
            return None;
        }
        let state = self.state.load();
        if !state.enabled {
            return None;
        }
        let model_lc = model.to_lowercase();
        let mut working = text.to_string();
        let mut changed = false;

        for cp in &state.text_policies {
            if cp.meta.mode != PolicyMode::Enforce {
                continue;
            }
            if !cp.rule.phase().applies_to(phase) {
                continue;
            }
            let ctx = EvalContext {
                phase,
                model: &model_lc,
                text: Cow::Borrowed(&working),
                json: None,
                input_tokens: None,
                live_state: None,
            };
            let outcome = cp.rule.evaluate(&ctx);
            if outcome.flagged && outcome.action.is_transform() {
                if let Some(t) = outcome.transformed_text {
                    if t != working {
                        working = t;
                        changed = true;
                    }
                }
            }
        }

        changed.then_some(working)
    }

    fn eval_cost_cap(
        &self,
        cc: &CostCapPolicy,
        model: &str,
        input_tokens: Option<u32>,
        max_output_tokens: Option<u64>,
        live_state: Option<&LiveState>,
    ) -> PolicyDecision {
        // Out-of-scope models pass.
        if !cost_cap_applies_to_model(cc, model) {
            return PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
        }

        // A model with no pricing entry has only the conservative assumed rate,
        // not a provider-published rate. For a fail-closed policy that means
        // "cannot meter exactly" → block; fail-open policies keep allowing while
        // reservation and settlement use the non-zero assumption.
        if cc.meta.fail_closed && crate::policy::pricing::lookup(model).is_none() {
            let mut d = PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
            d.flagged = true;
            d.score = 1.0;
            d.severity = Severity::Critical;
            d.action = cc.config.action;
            d.reason = format!(
                "model '{model}' has no pricing entry; cost cannot be metered; failing closed"
            );
            return d;
        }

        let window_key = window_label(cc.config.window);
        // A policy reads ONLY its own scope's counters. Substituting project
        // spend for a missing org counter is not a conservative fallback: it
        // silently rescopes the cap so every project may consume the whole org
        // allowance separately. A missing counter is missing state — the
        // unavailable-state decision below (fail-closed blocks, fail-open
        // allows with an explicit reason) is the honest answer.
        let spend = live_state.and_then(|s| {
            if cc.meta.org_scoped {
                s.org_cost_usd_by_window.get(window_key).copied()
            } else {
                s.cost_usd_by_window.get(window_key).copied()
            }
        });

        match spend {
            Some(spent) => {
                // Predict this request's own cost so an almost-exhausted cap
                // blocks *before* forwarding, not one request after. Unpriced
                // models predict None → 0 (fail-closed ones were rejected above).
                let est_request = crate::policy::pricing::estimate_request_cost(
                    model,
                    input_tokens.unwrap_or(0),
                    max_output_tokens,
                )
                .unwrap_or(0.0);
                let over_hard = spent >= cc.config.max_usd;
                let would_exceed = spent + est_request >= cc.config.max_usd;
                let over_soft = cc
                    .config
                    .soft_usd
                    .map(|soft| spent >= soft)
                    .unwrap_or(false);
                if over_hard || would_exceed {
                    let mut d =
                        PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
                    d.flagged = true;
                    d.score = 1.0;
                    d.severity = Severity::Critical;
                    d.action = cc.config.action;
                    d.reason = if over_hard {
                        format!(
                            "spend ${} over {window_key} reached cap ${}",
                            fmt_usd(spent),
                            fmt_usd(cc.config.max_usd)
                        )
                    } else {
                        format!(
                            "spend ${} over {window_key} plus estimated request cost ${} would exceed cap ${}",
                            fmt_usd(spent),
                            fmt_usd(est_request),
                            fmt_usd(cc.config.max_usd)
                        )
                    };
                    d
                } else if over_soft {
                    let mut d =
                        PolicyDecision::allow(&cc.meta.id, &cc.meta.name, "cost_cap", cc.meta.mode);
                    d.flagged = true;
                    d.score = 0.6;
                    d.severity = Severity::High;
                    d.action = PolicyAction::FlagOnly; // soft cap warns, never blocks
                    d.reason = format!(
                        "spend ${} over {window_key} crossed soft cap ${}",
                        fmt_usd(spent),
                        fmt_usd(cc.config.soft_usd.unwrap_or_default())
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
                // A policy reads ONLY its own scope's counters — an org policy
                // must never be satisfied by project counters (see
                // eval_cost_cap). A configured limit whose counter is absent is
                // unmeasurable, not satisfied: it takes the unavailable-state
                // decision rather than silently passing the check.
                let requests_for = |period: &str| {
                    if rl.meta.org_scoped {
                        state.org_requests_by_window.get(period).copied()
                    } else {
                        state.requests_by_window.get(period).copied()
                    }
                };
                let tokens_for = |period: &str| {
                    if rl.meta.org_scoped {
                        state.org_tokens_by_window.get(period).copied()
                    } else {
                        state.tokens_by_window.get(period).copied()
                    }
                };
                for w in &rl.config.windows {
                    if let Some(max) = w.max_requests {
                        let Some(count) = requests_for(&w.period) else {
                            return self.unavailable_state_decision(
                                &rl.meta,
                                "rate_limit",
                                w.action,
                                false,
                            );
                        };
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
                                format!("requests {count} reached limit {max} per {}", w.period);
                            return d;
                        }
                    }
                    if let Some(max) = w.max_tokens {
                        let Some(count) = tokens_for(&w.period) else {
                            return self.unavailable_state_decision(
                                &rl.meta,
                                "rate_limit",
                                w.action,
                                false,
                            );
                        };
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
        self.unavailable_decision_with_reason(
            meta,
            policy_type,
            action,
            strict,
            "live cost/rate state unavailable",
        )
    }

    /// [`PolicyEngine::unavailable_state_decision`] with an explicit cause, so
    /// an admission outage records *which* dependency was unavailable while
    /// keeping identical fail-closed semantics.
    fn unavailable_decision_with_reason(
        &self,
        meta: &PolicyMeta,
        policy_type: &str,
        action: PolicyAction,
        strict: bool,
        cause: &str,
    ) -> PolicyDecision {
        let fail_closed = meta.fail_closed || (strict && !self.fail_open_default);
        if fail_closed {
            let mut d = PolicyDecision::allow(&meta.id, &meta.name, policy_type, meta.mode);
            d.flagged = true;
            d.score = 1.0;
            d.severity = Severity::Critical;
            d.action = action;
            d.reason = format!("{cause}; failing closed");
            d
        } else {
            let mut d = PolicyDecision::allow(&meta.id, &meta.name, policy_type, meta.mode);
            d.reason = format!("{cause}; failing open");
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

    #[test]
    fn fmt_usd_keeps_precision_for_sub_cent_caps() {
        assert_eq!(fmt_usd(50.0), "50.00");
        assert_eq!(fmt_usd(0.01), "0.01");
        assert_eq!(fmt_usd(0.0), "0.00");
        // Sub-cent values must not collapse to "$0.00".
        assert_eq!(fmt_usd(0.001), "0.001");
        assert_eq!(fmt_usd(0.00001), "0.00001");
        assert_eq!(fmt_usd(0.000075), "0.000075");
        // Values that round to zero at 6 decimals collapse cleanly to "0.00"
        // (not a bare "0"), including negative zero.
        assert_eq!(fmt_usd(0.0000001), "0.00");
        assert_eq!(fmt_usd(-0.0), "0.00");
    }

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
    fn stateful_policy_count_isolates_live_state_backed_policies() {
        // Deployment targets with no live-state backend (the Cloudflare Worker)
        // key off this to refuse rather than silently no-op a cap.
        let text_only = engine(
            r#"{"policies":[{"name":"r","type":"regex_match","mode":"enforce",
            "config":{"phase":"input","patterns":[{"name":"x","regex":"x"}],"action":"block"}}]}"#,
        );
        assert_eq!(text_only.active_policy_count(), 1);
        assert_eq!(text_only.stateful_policy_count(), 0);

        let mixed = engine(
            r#"{"policies":[
            {"name":"r","type":"regex_match","mode":"enforce","config":{"phase":"input","patterns":[{"name":"x","regex":"x"}],"action":"block"}},
            {"name":"budget","type":"cost_cap","mode":"enforce","config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}},
            {"name":"rl","type":"rate_limit","mode":"enforce","config":{"windows":[{"period":"1m","maxRequests":5,"action":"block"}]}}
            ]}"#,
        );
        assert_eq!(mixed.active_policy_count(), 3);
        assert_eq!(mixed.stateful_policy_count(), 2);
    }

    /// Regression: a `failClosed` cap with an UNSET (therefore `advisory`)
    /// `enforcementMode` used to fail **open** when `/admit` was unavailable.
    ///
    /// `NOVEUM_GUARD_COST_ENFORCEMENT=strict` routes every cap through
    /// admission, but the fail-closed branch filtered on
    /// `enforcement_mode == Strict` and so saw none of them — the gateway
    /// forwarded the request to the provider unguarded. The platform's cost-cap
    /// schema leaves `enforcementMode` optional, so this was the *normal*
    /// configuration, not a corner case.
    #[test]
    fn admission_unavailable_considers_exactly_the_caps_admission_routed() {
        let unset_mode_fail_closed = backed_engine(
            r#"{"policies":[{"name":"fix-cap","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100000.0,"action":"block"}}]}"#,
        );
        assert!(
            !unset_mode_fail_closed.has_strict_cost_cap(),
            "the cap must be advisory for this test to reproduce the bug"
        );
        assert!(unset_mode_fail_closed
            .requires_explicit_output_limit("gpt-4o", Some(CostEnforcementMode::Strict)));
        assert!(
            unset_mode_fail_closed.requires_bounded_json_input(Some(CostEnforcementMode::Strict))
        );
        assert!(!unset_mode_fail_closed
            .requires_explicit_output_limit("gpt-4o", Some(CostEnforcementMode::Advisory)));
        assert!(!unset_mode_fail_closed
            .requires_bounded_json_input(Some(CostEnforcementMode::Advisory)));
        assert!(!unset_mode_fail_closed.requires_explicit_output_limit("gpt-4o", None));

        let forced = unset_mode_fail_closed
            .admission_unavailable_decision("503", "gpt-4o", true)
            .expect("a cap routed through admission by the override must be considered");
        assert!(
            forced.is_blocking(),
            "failClosed cap must block, got {forced:?}"
        );
        assert_eq!(forced.policy_id, "fix-cap");
        assert!(
            forced.reason.contains("failing closed"),
            "{}",
            forced.reason
        );

        assert!(
            unset_mode_fail_closed
                .admission_unavailable_decision("503", "gpt-4o", false)
                .is_none(),
            "without forceStrictCostCaps the platform excludes advisory caps"
        );

        let self_declared_strict = backed_engine(
            r#"{"policies":[{"name":"strict-cap","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100000.0,"action":"block","enforcementMode":"strict"}}]}"#,
        );
        assert!(self_declared_strict.has_strict_cost_cap());
        assert!(self_declared_strict.requires_explicit_output_limit("gpt-4o", None));
        assert!(self_declared_strict.requires_bounded_json_input(None));
        assert!(!self_declared_strict
            .requires_explicit_output_limit("gpt-4o", Some(CostEnforcementMode::Advisory)));
        let by_policy = self_declared_strict
            .admission_unavailable_decision("503", "gpt-4o", false)
            .expect("a self-declared strict cap is considered with no override");
        assert!(by_policy.is_blocking());
        assert_eq!(by_policy.policy_id, "strict-cap");

        let rate_only = backed_engine(
            r#"{"policies":[{"name":"rate","type":"rate_limit","mode":"enforce","failClosed":true,
            "config":{"windows":[{"period":"1m","maxRequests":5,"action":"block"}]}}]}"#,
        );
        assert!(
            !rate_only.requires_explicit_output_limit("gpt-4o", Some(CostEnforcementMode::Strict)),
            "a strict deployment override must not turn a rate-only policy into a cost cap"
        );
        assert!(!rate_only.requires_bounded_json_input(Some(CostEnforcementMode::Strict)));
        let unavailable_rate = rate_only
            .admission_unavailable_decision("503", "gpt-4o", false)
            .expect("the platform admission endpoint always evaluates rate limits");
        assert!(
            unavailable_rate.is_blocking(),
            "a failClosed rate limit must never become an implicit allow"
        );
        assert_eq!(unavailable_rate.policy_type, "rate_limit");

        let scoped = backed_engine(
            r#"{"policies":[{"name":"scoped","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block",
            "enforcementMode":"strict","scopeToModels":["GPT-4O"]}}]}"#,
        );
        assert!(scoped.requires_explicit_output_limit("gpt-4o", None));
        assert!(
            scoped.requires_bounded_json_input(None),
            "a non-JSON request cannot prove that it falls outside model scope"
        );
        assert!(
            !scoped.requires_explicit_output_limit("claude-sonnet-5", None),
            "a model outside scope must not be rejected"
        );
        assert!(
            scoped
                .admission_unavailable_decision("503", "claude-sonnet-5", false)
                .is_none(),
            "outage handling must not apply a strict cap to a model the platform filtered out"
        );
        assert!(scoped
            .admission_unavailable_decision("503", "GPT-4O", false)
            .expect("model matching is case-insensitive")
            .is_blocking());

        let shadow = backed_engine(
            r#"{"policies":[{"name":"shadow","type":"cost_cap","mode":"shadow",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block",
            "enforcementMode":"strict"}}]}"#,
        );
        assert!(
            !shadow.requires_explicit_output_limit("gpt-4o", None),
            "shadow mode must never turn into an active 400"
        );
        assert!(!shadow.requires_bounded_json_input(None));
    }

    /// The override widens *which* caps are considered, not what each one
    /// decides: a cap that is not `failClosed` still fails open, with the
    /// outage named in the reason.
    #[test]
    fn admission_unavailable_keeps_fail_open_for_a_non_fail_closed_cap() {
        let e = backed_engine(
            r#"{"policies":[{"name":"open-cap","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100000.0,"action":"block"}}]}"#,
        );
        let d = e
            .admission_unavailable_decision("admission unavailable (503)", "gpt-4o", true)
            .expect("considered under the strict override");
        assert!(!d.is_blocking(), "not failClosed, so it must fail open");
        assert!(
            d.reason.contains("platform admission unavailable"),
            "{}",
            d.reason
        );
        assert!(d.reason.contains("failing open"), "{}", d.reason);
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
    fn cost_cap_fail_closed_is_neutralized_without_state_backend() {
        // A `failClosed` cost_cap must NOT block when there is no live-state
        // backend (which is always, currently) — otherwise it would block 100%
        // of traffic forever. compile() neutralizes the flag with a warning.
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, None);
        assert!(
            !r.is_blocked(),
            "fail-closed cost_cap must fail open without a state backend"
        );
    }

    fn backed_engine(json: &str) -> PolicyEngine {
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        PolicyEngine::from_bundle(
            &bundle,
            EngineOptions {
                live_state_backed: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn cost_cap_fail_closed_blocks_unpriced_model() {
        // A model with no pricing entry can never advance the cost counters, so
        // a fail-closed cap must treat it as unmeterable and block.
        let e = backed_engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 0.0);
        let r = e.evaluate(
            Phase::Input,
            "totally-unknown-model",
            "hi",
            None,
            None,
            Some(&ls),
        );
        assert!(r.is_blocked(), "unpriced model must fail closed");
        assert!(r.block.unwrap().reason.contains("no pricing entry"));
        // Fail-open policies keep allowing unpriced models (advisory behavior).
        let open = backed_engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let r2 = open.evaluate(
            Phase::Input,
            "totally-unknown-model",
            "hi",
            None,
            None,
            Some(&ls),
        );
        assert!(!r2.is_blocked());
    }

    #[test]
    fn cost_cap_fail_closed_accepts_official_daybreak_aliases() {
        let e = backed_engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce","failClosed":true,
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 0.0);
        for model in ["daybreak-blue-latest", "daybreak-red-latest"] {
            let result = e.evaluate(Phase::Input, model, "hi", None, None, Some(&ls));
            assert!(
                !result.is_blocked(),
                "{model} is an active priced alias and must not hit the unknown-model fail-closed branch"
            );
        }
    }

    #[test]
    fn cost_cap_blocks_when_predicted_request_cost_would_exceed() {
        // Spend is *under* the cap, but this request's own predicted cost
        // (input tokens + max_tokens at gpt-4o rates) would push it over: the
        // request must be blocked BEFORE forwarding, not one request later.
        let e = engine(
            r#"{"policies":[{"name":"budget","type":"cost_cap","mode":"enforce",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 99.999);
        let body = serde_json::json!({"model":"gpt-4o","max_tokens": 1000});
        let r = e.evaluate(
            Phase::Input,
            "gpt-4o",
            "hi",
            Some(&body),
            Some(10),
            Some(&ls),
        );
        assert!(
            r.is_blocked(),
            "predicted request cost must pre-empt the cap"
        );
        assert!(r.block.unwrap().reason.contains("would exceed"));
        // Well under the cap the same request passes.
        let mut ls2 = LiveState::default();
        ls2.cost_usd_by_window.insert("30d_rolling".into(), 50.0);
        let r2 = e.evaluate(
            Phase::Input,
            "gpt-4o",
            "hi",
            Some(&body),
            Some(10),
            Some(&ls2),
        );
        assert!(!r2.is_blocked());
    }

    /// An engine wired to a live-state backend, as the platform bridge builds
    /// it. `failClosed` is only honored in this shape — without a state backend
    /// `compile` deliberately neutralizes it (nothing could ever satisfy it).
    fn live_state_engine(json: &str) -> PolicyEngine {
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        PolicyEngine::from_bundle(
            &bundle,
            EngineOptions {
                live_state_backed: true,
                ..Default::default()
            },
        )
    }

    /// An org-scoped `cost_cap` at $100 over `30d_rolling`, on a live-state
    /// backed engine. `fail_closed` selects the variant of the same policy.
    fn org_cost_cap_engine(fail_closed: bool) -> PolicyEngine {
        live_state_engine(&format!(
            r#"{{"policies":[{{"name":"orgcap","type":"cost_cap","mode":"enforce","source":"org",
            "failClosed":{fail_closed},
            "config":{{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}}}]}}"#
        ))
    }

    #[test]
    fn org_sourced_cost_cap_reads_org_counters() {
        let e = org_cost_cap_engine(false);
        // Project spend is under the cap, org spend is over: must block on org.
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 10.0);
        ls.org_cost_usd_by_window
            .insert("30d_rolling".into(), 150.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(r.is_blocked(), "org policy must enforce against org spend");
        // ...and the reverse: org under cap, project over. The org policy must
        // read org state and allow, not block on the project's spend.
        let mut ls2 = LiveState::default();
        ls2.cost_usd_by_window.insert("30d_rolling".into(), 150.0);
        ls2.org_cost_usd_by_window.insert("30d_rolling".into(), 1.0);
        let r2 = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls2));
        assert!(!r2.is_blocked(), "org policy must not read project spend");
    }

    #[test]
    fn missing_org_cost_window_never_substitutes_project_cost() {
        // Substituting project spend would rescope the cap: every project could
        // then consume the full org allowance separately. A missing org counter
        // is missing state, so a fail-open policy allows with that reason...
        let e = org_cost_cap_engine(false);
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 150.0); // over cap
        ls.org_requests_by_window.insert("1m".into(), 1); // org section present, wrong kind
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(!r.is_blocked(), "fail-open unavailable state must allow");
        assert!(
            r.decisions.iter().any(|d| d.reason.contains("unavailable")),
            "the allow must be recorded as unavailable state, not as a pass"
        );
        // ...and an org section carrying only *other* windows behaves the same:
        // the per-window key is what's missing, and it is not substitutable.
        let mut ls2 = LiveState::default();
        ls2.org_cost_usd_by_window.insert("1d_rolling".into(), 1.0);
        ls2.cost_usd_by_window.insert("30d_rolling".into(), 150.0);
        let r2 = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls2));
        assert!(!r2.is_blocked());
        assert!(r2
            .decisions
            .iter()
            .any(|d| d.reason.contains("unavailable")));
    }

    #[test]
    fn fail_closed_missing_org_cost_state_blocks() {
        let e = org_cost_cap_engine(true);
        // Project spend known and *under* cap; org counter absent. Fail-closed
        // must block rather than borrow the project's reassuring number.
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 1.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(r.is_blocked(), "fail-closed missing org state must block");
        assert!(r.block.unwrap().reason.contains("unavailable"));
    }

    #[test]
    fn missing_org_rate_counters_use_unavailable_semantics() {
        // A configured limit whose counter is absent is unmeasurable, not
        // satisfied — the old code skipped the check and allowed outright.
        for (kind, config) in [
            (
                "requests",
                r#"{"windows":[{"period":"1m","maxRequests":60,"action":"block"}]}"#,
            ),
            (
                "tokens",
                r#"{"windows":[{"period":"1m","maxTokens":1000,"action":"block"}]}"#,
            ),
        ] {
            // Fail-open: allowed, but recorded as unavailable state.
            let open = engine(&format!(
                r#"{{"policies":[{{"name":"orgrl","type":"rate_limit","mode":"enforce","source":"org",
                "config":{config}}}]}}"#
            ));
            // Project counters are populated and over the limit; the org policy
            // must not read them.
            let mut ls = LiveState::default();
            ls.requests_by_window.insert("1m".into(), 100);
            ls.tokens_by_window.insert("1m".into(), 10_000);
            let r = open.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
            assert!(!r.is_blocked(), "{kind}: fail-open must allow");
            assert!(
                r.decisions.iter().any(|d| d.reason.contains("unavailable")),
                "{kind}: missing org counter must report unavailable state"
            );

            // Fail-closed: the same missing counter blocks.
            let closed = live_state_engine(&format!(
                r#"{{"policies":[{{"name":"orgrl","type":"rate_limit","mode":"enforce","source":"org",
                "failClosed":true,"config":{config}}}]}}"#
            ));
            let r2 = closed.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
            assert!(r2.is_blocked(), "{kind}: fail-closed must block");
            assert!(r2.block.unwrap().reason.contains("unavailable"));
        }
    }

    #[test]
    fn project_policies_are_unaffected_by_org_counters() {
        // The no-substitution rule runs both ways: a project policy reads only
        // project counters even when org counters are present and over cap.
        let e = engine(
            r#"{"policies":[{"name":"projcap","type":"cost_cap","mode":"enforce","source":"project",
            "config":{"window":"30d_rolling","maxUsd":100.0,"action":"block"}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.cost_usd_by_window.insert("30d_rolling".into(), 10.0);
        ls.org_cost_usd_by_window
            .insert("30d_rolling".into(), 5_000.0);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(!r.is_blocked(), "project cap must not read org spend");
    }

    #[test]
    fn org_sourced_rate_limit_reads_org_counters() {
        let e = engine(
            r#"{"policies":[{"name":"orgrl","type":"rate_limit","mode":"enforce","source":"organization",
            "config":{"windows":[{"period":"1m","maxRequests":60,"action":"block"}]}}]}"#,
        );
        let mut ls = LiveState::default();
        ls.requests_by_window.insert("1m".into(), 1);
        ls.org_requests_by_window.insert("1m".into(), 100);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, Some(&ls));
        assert!(r.is_blocked());
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
    fn apply_text_transforms_only_runs_transforms_not_blocks() {
        // A block policy + a redact policy. apply_text_transforms must apply the
        // redact and ignore the block (no re-litigating blocking per segment).
        let e = engine(
            r#"{"policies":[
              {"name":"blk","type":"regex_match","mode":"enforce","priority":10,
               "config":{"phase":"input","patterns":[{"name":"ssn","regex":"\\d{3}-\\d{2}-\\d{4}"}],"action":"block"}},
              {"name":"red","type":"pii_detection","mode":"enforce","priority":20,
               "config":{"phase":"input","entities":["EMAIL_ADDRESS"],"action":"redact"}}
            ]}"#,
        );
        // Input contains an SSN (which the block policy WOULD match) plus an
        // email. apply_text_transforms must ignore the block and only redact the
        // email — proving blocking is not re-litigated here. The SSN is left as-is
        // because the block rule is not a transform.
        let out = e.apply_text_transforms(Phase::Input, "gpt-4o", "ssn 123-45-6789 mail a@b.com");
        assert_eq!(out.as_deref(), Some("ssn 123-45-6789 mail [REDACTED]"));
        // text with no email -> no transform
        assert!(e
            .apply_text_transforms(Phase::Input, "gpt-4o", "nothing here")
            .is_none());
    }

    #[test]
    fn apply_text_transforms_skips_shadow_mode() {
        let e = engine(
            r#"{"policies":[{"name":"red","type":"pii_detection","mode":"shadow",
            "config":{"phase":"input","entities":["EMAIL_ADDRESS"],"action":"redact"}}]}"#,
        );
        assert!(e
            .apply_text_transforms(Phase::Input, "gpt-4o", "mail a@b.com")
            .is_none());
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
    fn unknown_policy_type_is_a_visible_rejection_not_a_silent_skip() {
        let e = engine(
            r#"{"policies":[{"name":"typo","type":"promt_injection","mode":"enforce","config":{}}]}"#,
        );
        assert_eq!(e.active_policy_count(), 0);
        assert_eq!(e.rejected_policy_count(), 1);
        let msgs = e.rejected_policies();
        assert!(msgs[0].contains("promt_injection"), "{msgs:?}");
        assert!(msgs[0].contains("unknown policy type"), "{msgs:?}");
        // Fail-open by default: a typo must not take production down.
        let r = e.evaluate(Phase::Input, "gpt-4o", "hi", None, None, None);
        assert!(!r.is_blocked());
    }

    #[test]
    fn invalid_config_is_rejected_rather_than_skipped_with_a_warning() {
        // A regex_match with no patterns used to compile to nothing and be
        // dropped with a `warn!`, so the operator's guardrail silently did
        // nothing. It is now a counted, named rejection.
        let e = engine(
            r#"{"policies":[{"name":"empty","type":"regex_match","mode":"enforce","config":{}}]}"#,
        );
        assert_eq!(e.active_policy_count(), 0);
        assert_eq!(e.rejected_policy_count(), 1);
        assert!(e.rejected_policies()[0].contains("patterns"));
    }

    #[test]
    fn reserved_classifier_type_is_rejected_with_a_reason() {
        let e = engine(
            r#"{"policies":[{"name":"inj","type":"prompt_injection","mode":"enforce",
            "config":{"threshold":0.9,"action":"block"}}]}"#,
        );
        assert_eq!(e.active_policy_count(), 0);
        let msgs = e.rejected_policies();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("prompt_injection"), "{msgs:?}");
        assert!(msgs[0].contains("external scoring service"), "{msgs:?}");
    }

    #[test]
    fn fail_closed_rejection_blocks_every_request() {
        // `failClosed` means "block when this policy cannot be evaluated". A
        // policy that will never compile can never be evaluated, so failing open
        // would deliver exactly what the operator wrote failClosed to prevent.
        let e = engine(
            r#"{"policies":[{"name":"critical","type":"content_moderation","mode":"enforce",
            "failClosed":true,"config":{"categories":["hate"]}}]}"#,
        );
        assert_eq!(e.rejected_policy_count(), 1);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hello", None, None, None);
        assert!(r.is_blocked(), "fail-closed rejection must block");
        let b = r.block.unwrap();
        assert_eq!(b.policy_type, "content_moderation");
        assert!(b.reason.contains("failing closed"), "{}", b.reason);
        assert_eq!(b.action, PolicyAction::Block);
    }

    #[test]
    fn fail_closed_rejection_counts_as_active_so_the_guard_still_runs() {
        // `middleware.rs` and `worker_rt.rs` skip the whole guard path when
        // `active_policy_count() == 0`. A rejection that blocks every request has
        // to be visible to that gate, or the block would never be applied.
        let blocking = engine(
            r#"{"policies":[{"name":"m","type":"content_moderation","mode":"enforce",
            "failClosed":true,"config":{"categories":["hate"]}}]}"#,
        );
        assert_eq!(blocking.active_policy_count(), 1);
        // A fail-open rejection changes nothing about request handling, so it
        // must NOT switch the guard on for a deployment that has no policies.
        let inert = engine(
            r#"{"policies":[{"name":"m","type":"content_moderation","mode":"enforce",
            "config":{"categories":["hate"]}}]}"#,
        );
        assert_eq!(inert.active_policy_count(), 0);
        assert_eq!(inert.rejected_policy_count(), 1);
    }

    #[test]
    fn shadow_mode_rejection_never_blocks() {
        // Shadow means "record, never apply". That has to hold for a compile
        // failure too, or a shadow rollout could take production down.
        let e = engine(
            r#"{"policies":[{"name":"critical","type":"content_moderation","mode":"shadow",
            "failClosed":true,"config":{"categories":["hate"]}}]}"#,
        );
        assert_eq!(e.rejected_policy_count(), 1);
        let r = e.evaluate(Phase::Input, "gpt-4o", "hello", None, None, None);
        assert!(!r.is_blocked());
    }

    #[test]
    fn rejections_do_not_stop_valid_policies_from_compiling() {
        let e = engine(
            r#"{"policies":[
              {"name":"bad","type":"quantum_check","mode":"enforce","config":{}},
              {"name":"good","type":"regex_match","mode":"enforce",
               "config":{"phase":"input","patterns":[{"name":"a","regex":"boom"}],"action":"block"}}
            ]}"#,
        );
        assert_eq!(e.active_policy_count(), 1);
        assert_eq!(e.rejected_policy_count(), 1);
        assert!(e
            .evaluate(Phase::Input, "gpt-4o", "boom", None, None, None)
            .is_blocked());
    }

    #[test]
    fn swap_bundle_clears_stale_rejections() {
        let e = engine(r#"{"policies":[{"name":"bad","type":"nope","config":{}}]}"#);
        assert_eq!(e.rejected_policy_count(), 1);
        let fixed = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"good","type":"model_allowlist","mode":"enforce","config":{"allowed":["gpt-4o"]}}]}"#,
        )
        .unwrap();
        e.swap_bundle(&fixed);
        assert_eq!(e.rejected_policy_count(), 0);
        assert_eq!(e.active_policy_count(), 1);
    }

    #[test]
    fn empty_live_state_struct_constructs() {
        let ls = LiveState::default();
        assert!(ls.cost_usd_by_window.is_empty());
        assert!(!ls.has_org_counters());
    }
}
