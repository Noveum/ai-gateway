//! Core decision types shared by every Nova Guard policy.
//!
//! A [`PolicyDecision`] is the uniform result of evaluating one policy against
//! one request/response payload. It is intentionally provider- and rule-agnostic
//! so the engine, the middleware, and the telemetry exporter can all reason about
//! decisions uniformly, and so decisions serialize cleanly into OpenTelemetry span
//! events (`noveum.guard.policy_decision`) that match the Nova Guard SDK schema.

use serde::{Deserialize, Serialize};

/// Which side of the LLM call a policy inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The request the application sends to the model (prompt, messages, tools).
    Input,
    /// The response the model returns (completion text, tool calls).
    Output,
    /// Both phases.
    Both,
}

impl Phase {
    /// Does this policy's phase apply to the given evaluation phase?
    ///
    /// `Both` matches everything; `Input`/`Output` match only themselves.
    pub fn applies_to(self, eval_phase: Phase) -> bool {
        matches!(
            (self, eval_phase),
            (Phase::Both, _)
                | (_, Phase::Both)
                | (Phase::Input, Phase::Input)
                | (Phase::Output, Phase::Output)
        )
    }
}

/// How aggressively a policy is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PolicyMode {
    /// Not evaluated at all.
    Off,
    /// Evaluated and logged, but never blocks or mutates the payload.
    #[default]
    Shadow,
    /// Evaluated and applied: may block or mutate the payload.
    Enforce,
}

/// What a policy decided to do with the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Allow the payload through unchanged.
    #[default]
    Allow,
    /// Reject the call entirely (short-circuit with a synthetic block response).
    Block,
    /// Remove the matched spans from the payload.
    Redact,
    /// Replace the matched spans with a masking character (e.g. `****`).
    Mask,
    /// Replace the matched spans with a hash of their content.
    Hash,
    /// Replace the matched spans with a fixed replacement string.
    Replace,
    /// Record the decision but take no action on the payload (detect-only).
    FlagOnly,
}

impl PolicyAction {
    /// Is this action one that mutates the payload text?
    pub fn is_transform(self) -> bool {
        matches!(
            self,
            PolicyAction::Redact | PolicyAction::Mask | PolicyAction::Hash | PolicyAction::Replace
        )
    }

    /// Is this action terminal (aborts the chain and blocks the call)?
    pub fn is_block(self) -> bool {
        matches!(self, PolicyAction::Block)
    }
}

/// Coarse severity bucket derived from a policy's numeric score and config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Map a normalized score in `[0.0, 1.0]` onto a severity bucket.
    pub fn from_score(score: f32) -> Self {
        match score {
            s if s <= 0.0 => Severity::None,
            s if s < 0.25 => Severity::Low,
            s if s < 0.5 => Severity::Medium,
            s if s < 0.85 => Severity::High,
            _ => Severity::Critical,
        }
    }
}

/// The uniform result of evaluating one policy against one payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub policy_id: String,
    pub policy_name: String,
    pub policy_type: String,
    pub mode: PolicyMode,
    /// Normalized violation strength, `0.0` (clean) .. `1.0` (strongest).
    pub score: f32,
    pub severity: Severity,
    /// `true` when the policy detected a violation (independent of whether it acted).
    pub flagged: bool,
    pub action: PolicyAction,
    /// The mutated payload text, present only for transform actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transformed_text: Option<String>,
    /// Human-readable explanation, safe for logs (never contains the raw match).
    pub reason: String,
    /// Entity/rule labels that matched (e.g. `["US_SSN", "EMAIL_ADDRESS"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_entities: Vec<String>,
    /// Wall-clock evaluation time in milliseconds.
    pub latency_ms: f64,
}

impl PolicyDecision {
    /// An allow decision for a policy that did not fire.
    pub fn allow(
        policy_id: impl Into<String>,
        policy_name: impl Into<String>,
        policy_type: impl Into<String>,
        mode: PolicyMode,
    ) -> Self {
        Self {
            policy_id: policy_id.into(),
            policy_name: policy_name.into(),
            policy_type: policy_type.into(),
            mode,
            score: 0.0,
            severity: Severity::None,
            flagged: false,
            action: PolicyAction::Allow,
            transformed_text: None,
            reason: String::new(),
            matched_entities: Vec::new(),
            latency_ms: 0.0,
        }
    }

    /// Does this decision short-circuit the request?
    ///
    /// Only `Enforce`-mode `Block` decisions short-circuit. `Shadow` mode never
    /// blocks regardless of action.
    pub fn is_blocking(&self) -> bool {
        self.mode == PolicyMode::Enforce && self.action.is_block()
    }

    /// Does this decision mutate the payload?
    ///
    /// Only `Enforce`-mode transform decisions mutate. `Shadow` mode records the
    /// would-be transform in `transformed_text` for the dashboard but does not
    /// apply it.
    pub fn is_applied_transform(&self) -> bool {
        self.mode == PolicyMode::Enforce
            && self.action.is_transform()
            && self.transformed_text.is_some()
    }

    /// Flatten into OpenTelemetry span-event attributes (`noveum.guard.*`),
    /// matching the Nova Guard SDK telemetry schema. Never includes raw matched
    /// content — only entity types and offsets-free reasons.
    pub fn to_span_attributes(&self) -> serde_json::Value {
        serde_json::json!({
            "noveum.guard.policy_id": self.policy_id,
            "noveum.guard.policy_name": self.policy_name,
            "noveum.guard.policy_type": self.policy_type,
            "noveum.guard.mode": self.mode,
            "noveum.guard.action": self.action,
            "noveum.guard.flagged": self.flagged,
            "noveum.guard.score": self.score,
            "noveum.guard.severity": self.severity,
            "noveum.guard.reason": self.reason,
            "noveum.guard.matched_entities": self.matched_entities,
            "noveum.guard.latency_ms": self.latency_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_applies_to() {
        assert!(Phase::Both.applies_to(Phase::Input));
        assert!(Phase::Both.applies_to(Phase::Output));
        assert!(Phase::Input.applies_to(Phase::Input));
        assert!(!Phase::Input.applies_to(Phase::Output));
        assert!(Phase::Output.applies_to(Phase::Output));
        assert!(!Phase::Output.applies_to(Phase::Input));
        // an Input policy is considered when the engine evaluates "both"
        assert!(Phase::Input.applies_to(Phase::Both));
    }

    #[test]
    fn action_classification() {
        assert!(PolicyAction::Block.is_block());
        assert!(!PolicyAction::Mask.is_block());
        assert!(PolicyAction::Mask.is_transform());
        assert!(PolicyAction::Redact.is_transform());
        assert!(PolicyAction::Hash.is_transform());
        assert!(PolicyAction::Replace.is_transform());
        assert!(!PolicyAction::Block.is_transform());
        assert!(!PolicyAction::Allow.is_transform());
        assert!(!PolicyAction::FlagOnly.is_transform());
    }

    #[test]
    fn severity_buckets() {
        assert_eq!(Severity::from_score(0.0), Severity::None);
        assert_eq!(Severity::from_score(-1.0), Severity::None);
        assert_eq!(Severity::from_score(0.1), Severity::Low);
        assert_eq!(Severity::from_score(0.3), Severity::Medium);
        assert_eq!(Severity::from_score(0.6), Severity::High);
        assert_eq!(Severity::from_score(0.9), Severity::Critical);
        assert_eq!(Severity::from_score(1.0), Severity::Critical);
    }

    #[test]
    fn shadow_mode_never_blocks_or_transforms() {
        let mut d = PolicyDecision::allow("p1", "name", "regex_match", PolicyMode::Shadow);
        d.action = PolicyAction::Block;
        assert!(!d.is_blocking(), "shadow block must not short-circuit");

        d.action = PolicyAction::Mask;
        d.transformed_text = Some("masked".into());
        assert!(!d.is_applied_transform(), "shadow transform must not apply");
    }

    #[test]
    fn enforce_mode_blocks_and_transforms() {
        let mut d = PolicyDecision::allow("p1", "name", "regex_match", PolicyMode::Enforce);
        d.action = PolicyAction::Block;
        assert!(d.is_blocking());

        d.action = PolicyAction::Mask;
        d.transformed_text = Some("masked".into());
        assert!(d.is_applied_transform());

        // transform without transformed_text does not count as applied
        d.transformed_text = None;
        assert!(!d.is_applied_transform());
    }

    #[test]
    fn span_attributes_have_namespaced_keys() {
        let d = PolicyDecision::allow("p1", "Block SSN", "regex_match", PolicyMode::Enforce);
        let attrs = d.to_span_attributes();
        assert_eq!(attrs["noveum.guard.policy_id"], "p1");
        assert_eq!(attrs["noveum.guard.policy_type"], "regex_match");
        assert_eq!(attrs["noveum.guard.flagged"], false);
    }

    #[test]
    fn modes_and_actions_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&PolicyMode::Enforce).unwrap(),
            "\"enforce\""
        );
        assert_eq!(serde_json::to_string(&Phase::Input).unwrap(), "\"input\"");
        assert_eq!(
            serde_json::to_string(&PolicyAction::FlagOnly).unwrap(),
            "\"flag_only\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
    }
}
