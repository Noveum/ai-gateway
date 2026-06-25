//! The extensible rule layer.
//!
//! Every policy type implements [`PolicyRule`]. A rule is constructed from a
//! [`Policy`] definition (parsing its type-specific `config`) and, at request
//! time, produces a [`RuleOutcome`] describing what it detected and what action
//! it would take. The engine wraps that outcome with the policy's mode
//! (shadow/enforce) to produce the final [`PolicyDecision`](super::decision::PolicyDecision).
//!
//! Adding a new policy type is intentionally local: implement [`PolicyRule`],
//! add a `parse` arm in [`compile_rule`], and the engine, middleware, and
//! telemetry pick it up with no further changes.

use std::borrow::Cow;

use serde_json::Value;
use tracing::warn;

use super::config::Policy;
use super::config::PolicyType;
use super::decision::{Phase, PolicyAction};

pub mod banned_substrings;
pub mod json_schema;
pub mod model_allowlist;
pub mod pii;
pub mod regex_match;
pub mod secrets;
pub mod token_length_cap;

/// Live cost/rate counters supplied by the control plane (or a local fallback),
/// used by `cost_cap` and `rate_limit`. Absent when the gateway runs standalone
/// with no control plane configured.
#[derive(Debug, Clone, Default)]
pub struct LiveState {
    /// Spend in USD keyed by window label, e.g. `"30d_rolling" -> 1247.81`.
    pub cost_usd_by_window: std::collections::HashMap<String, f64>,
    /// Request counts keyed by window label, e.g. `"1m" -> 42`.
    pub requests_by_window: std::collections::HashMap<String, u64>,
    /// Token counts keyed by window label.
    pub tokens_by_window: std::collections::HashMap<String, u64>,
}

/// Everything a rule may inspect for one evaluation.
pub struct EvalContext<'a> {
    /// The phase being evaluated (`Input` or `Output`).
    pub phase: Phase,
    /// The model id from the request body, lowercased.
    pub model: &'a str,
    /// The flattened text to scan (prompt text for input, completion text for
    /// output). Rules that transform return the mutated text in their outcome;
    /// the engine threads it into subsequent rules.
    pub text: Cow<'a, str>,
    /// The full request or response JSON, for schema/field checks.
    pub json: Option<&'a Value>,
    /// Estimated/known input token count, when available.
    pub input_tokens: Option<u32>,
    /// Live cost/rate state, when a control plane is configured.
    pub live_state: Option<&'a LiveState>,
}

/// What a rule detected, before mode (shadow/enforce) is applied.
#[derive(Debug, Clone)]
pub struct RuleOutcome {
    /// Did the rule detect a violation?
    pub flagged: bool,
    /// Normalized violation strength `[0.0, 1.0]`.
    pub score: f32,
    /// The action to take when flagged (the rule's configured action). `Allow`
    /// when not flagged.
    pub action: PolicyAction,
    /// Mutated text for transform actions.
    pub transformed_text: Option<String>,
    /// Log-safe explanation (never the raw matched value).
    pub reason: String,
    /// Matched entity/rule labels.
    pub matched_entities: Vec<String>,
}

impl RuleOutcome {
    /// A clean (non-flagged) outcome.
    pub fn clean() -> Self {
        Self {
            flagged: false,
            score: 0.0,
            action: PolicyAction::Allow,
            transformed_text: None,
            reason: String::new(),
            matched_entities: Vec::new(),
        }
    }

    /// A flagged outcome with the given action, score, and reason.
    pub fn flagged(action: PolicyAction, score: f32, reason: impl Into<String>) -> Self {
        Self {
            flagged: true,
            score: score.clamp(0.0, 1.0),
            action,
            transformed_text: None,
            reason: reason.into(),
            matched_entities: Vec::new(),
        }
    }

    pub fn with_entities(mut self, entities: Vec<String>) -> Self {
        self.matched_entities = entities;
        self
    }

    pub fn with_transformed(mut self, text: String) -> Self {
        self.transformed_text = Some(text);
        self
    }
}

/// A compiled, ready-to-run policy rule.
pub trait PolicyRule: Send + Sync {
    /// The policy type this rule implements.
    fn policy_type(&self) -> PolicyType;

    /// Which phase this rule inspects.
    fn phase(&self) -> Phase;

    /// Evaluate against the payload.
    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome;
}

/// Compile a [`Policy`] definition into a runnable rule.
///
/// Returns `None` (with a warning) for unknown types, types whose config fails
/// to parse, or types that are not enforced in-process (the v1.5 scorer-gate
/// family, which the engine routes to the NovaEval scoring service separately).
pub fn compile_rule(policy: &Policy) -> Option<Box<dyn PolicyRule>> {
    let cfg = policy.config.clone();
    let result: Result<Box<dyn PolicyRule>, String> = match policy.policy_type {
        PolicyType::RegexMatch => {
            regex_match::RegexMatchRule::parse(cfg).map(|r| Box::new(r) as Box<dyn PolicyRule>)
        }
        PolicyType::BannedSubstrings => banned_substrings::BannedSubstringsRule::parse(cfg)
            .map(|r| Box::new(r) as Box<dyn PolicyRule>),
        PolicyType::ModelAllowlist => model_allowlist::ModelAllowlistRule::parse(cfg)
            .map(|r| Box::new(r) as Box<dyn PolicyRule>),
        PolicyType::TokenLengthCap => token_length_cap::TokenLengthCapRule::parse(cfg)
            .map(|r| Box::new(r) as Box<dyn PolicyRule>),
        PolicyType::JsonSchema => {
            json_schema::JsonSchemaRule::parse(cfg).map(|r| Box::new(r) as Box<dyn PolicyRule>)
        }
        PolicyType::PiiDetection => {
            pii::PiiRule::parse(cfg).map(|r| Box::new(r) as Box<dyn PolicyRule>)
        }
        PolicyType::SecretsDetection => {
            secrets::SecretsRule::parse(cfg).map(|r| Box::new(r) as Box<dyn PolicyRule>)
        }
        // cost_cap and rate_limit are handled by the engine's live-state path,
        // not as text rules; scorer-gate family is routed to NovaEval.
        other => {
            warn!(
                policy = %policy.id(),
                policy_type = other.as_str(),
                "policy type not enforced in-process by the gateway rule layer; skipping (will be routed to scorer-gate or is engine-handled)"
            );
            return None;
        }
    };

    match result {
        Ok(rule) => Some(rule),
        Err(e) => {
            warn!(
                policy = %policy.id(),
                policy_type = policy.policy_type.as_str(),
                error = %e,
                "failed to compile policy config; skipping this policy"
            );
            None
        }
    }
}

/// Helper: extract a lowercased `model` field from a request JSON body.
pub fn model_from_json(json: &Value) -> Option<String> {
    json.get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::config::PolicyBundle;

    #[test]
    fn compile_known_deterministic_rule() {
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"a","type":"model_allowlist","config":{"allowed":["gpt-4o"]}}]}"#,
        )
        .unwrap();
        let rule = compile_rule(&bundle.policies[0]);
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().policy_type(), PolicyType::ModelAllowlist);
    }

    #[test]
    fn compile_unknown_returns_none() {
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"a","type":"made_up_type","config":{}}]}"#,
        )
        .unwrap();
        assert!(compile_rule(&bundle.policies[0]).is_none());
    }

    #[test]
    fn compile_scorer_gate_returns_none_routed_elsewhere() {
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"a","type":"scorer_gate","config":{}}]}"#,
        )
        .unwrap();
        assert!(compile_rule(&bundle.policies[0]).is_none());
    }

    #[test]
    fn compile_bad_config_returns_none() {
        // token_length_cap requires max_tokens; omitting it should fail to parse.
        let bundle = PolicyBundle::from_json_str(
            r#"{"policies":[{"name":"a","type":"token_length_cap","config":{}}]}"#,
        )
        .unwrap();
        assert!(compile_rule(&bundle.policies[0]).is_none());
    }

    #[test]
    fn outcome_constructors() {
        let clean = RuleOutcome::clean();
        assert!(!clean.flagged);
        assert_eq!(clean.action, PolicyAction::Allow);

        let flagged =
            RuleOutcome::flagged(PolicyAction::Block, 2.0, "boom").with_entities(vec!["X".into()]);
        assert!(flagged.flagged);
        assert_eq!(flagged.score, 1.0); // clamped
        assert_eq!(flagged.matched_entities, vec!["X".to_string()]);
    }

    #[test]
    fn model_from_json_lowercases() {
        let v = serde_json::json!({"model": "GPT-4O"});
        assert_eq!(model_from_json(&v).as_deref(), Some("gpt-4o"));
        assert_eq!(model_from_json(&serde_json::json!({})), None);
    }
}
