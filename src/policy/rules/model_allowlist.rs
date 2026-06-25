//! `model_allowlist` — restrict which models may be called.
//!
//! If an `allowed` list is non-empty, only those models pass. A `denied` list
//! blocks specific models regardless of the allowlist. Matching is
//! case-insensitive and supports a trailing `*` wildcard (e.g. `gpt-4*`).

use serde_json::Value;

use crate::policy::config::{ModelAllowlistConfig, PolicyType};
use crate::policy::decision::Phase;

use super::{EvalContext, PolicyRule, RuleOutcome};

pub struct ModelAllowlistRule {
    allowed: Vec<String>,
    denied: Vec<String>,
    action: crate::policy::decision::PolicyAction,
}

impl ModelAllowlistRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: ModelAllowlistConfig = serde_json::from_value(cfg)
            .map_err(|e| format!("invalid model_allowlist config: {e}"))?;
        if cfg.allowed.is_empty() && cfg.denied.is_empty() {
            return Err("model_allowlist requires a non-empty `allowed` or `denied` list".into());
        }
        Ok(Self {
            allowed: cfg.allowed.iter().map(|s| s.to_lowercase()).collect(),
            denied: cfg.denied.iter().map(|s| s.to_lowercase()).collect(),
            action: cfg.action,
        })
    }
}

/// Case-insensitive match with optional trailing `*` wildcard.
fn matches(pattern: &str, model: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        model.starts_with(prefix)
    } else {
        pattern == model
    }
}

impl PolicyRule for ModelAllowlistRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::ModelAllowlist
    }

    fn phase(&self) -> Phase {
        // Model selection is an input-phase concern.
        Phase::Input
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let model = ctx.model;

        // Denylist takes precedence.
        if self.denied.iter().any(|d| matches(d, model)) {
            return RuleOutcome::flagged(
                self.action,
                1.0,
                format!("model '{model}' is explicitly denied"),
            )
            .with_entities(vec![model.to_string()]);
        }

        // If an allowlist is configured, the model must be on it.
        if !self.allowed.is_empty() && !self.allowed.iter().any(|a| matches(a, model)) {
            return RuleOutcome::flagged(
                self.action,
                1.0,
                format!("model '{model}' is not in the allowlist"),
            )
            .with_entities(vec![model.to_string()]);
        }

        RuleOutcome::clean()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::decision::PolicyAction;
    use std::borrow::Cow;

    fn ctx_for(model: &str) -> EvalContext<'static> {
        EvalContext {
            phase: Phase::Input,
            model: Box::leak(model.to_string().into_boxed_str()),
            text: Cow::Borrowed(""),
            json: None,
            input_tokens: None,
            live_state: None,
        }
    }

    #[test]
    fn allows_listed_model() {
        let r = ModelAllowlistRule::parse(
            serde_json::json!({"allowed": ["gpt-4o", "claude-haiku-4-5"]}),
        )
        .unwrap();
        assert!(!r.evaluate(&ctx_for("gpt-4o")).flagged);
        assert!(!r.evaluate(&ctx_for("claude-haiku-4-5")).flagged);
    }

    #[test]
    fn blocks_unlisted_model() {
        let r = ModelAllowlistRule::parse(serde_json::json!({"allowed": ["gpt-4o"]})).unwrap();
        let out = r.evaluate(&ctx_for("gpt-4-turbo"));
        assert!(out.flagged);
        assert_eq!(out.action, PolicyAction::Block);
    }

    #[test]
    fn wildcard_prefix_matches() {
        let r = ModelAllowlistRule::parse(serde_json::json!({"allowed": ["gpt-4*"]})).unwrap();
        assert!(!r.evaluate(&ctx_for("gpt-4o")).flagged);
        assert!(!r.evaluate(&ctx_for("gpt-4-turbo")).flagged);
        assert!(r.evaluate(&ctx_for("gpt-3.5-turbo")).flagged);
    }

    #[test]
    fn denylist_overrides_allowlist() {
        let r = ModelAllowlistRule::parse(
            serde_json::json!({"allowed": ["gpt-4*"], "denied": ["gpt-4-turbo"]}),
        )
        .unwrap();
        assert!(!r.evaluate(&ctx_for("gpt-4o")).flagged);
        assert!(r.evaluate(&ctx_for("gpt-4-turbo")).flagged);
    }

    #[test]
    fn case_insensitive() {
        let r = ModelAllowlistRule::parse(serde_json::json!({"allowed": ["GPT-4o"]})).unwrap();
        assert!(!r.evaluate(&ctx_for("gpt-4o")).flagged);
    }

    #[test]
    fn empty_config_rejected() {
        assert!(ModelAllowlistRule::parse(serde_json::json!({})).is_err());
    }

    #[test]
    fn denylist_only_blocks_named() {
        let r = ModelAllowlistRule::parse(serde_json::json!({"denied": ["o1-preview"]})).unwrap();
        assert!(r.evaluate(&ctx_for("o1-preview")).flagged);
        assert!(!r.evaluate(&ctx_for("gpt-4o")).flagged); // not denied, no allowlist
    }
}
