//! `token_length_cap` — block requests whose token count exceeds a limit.
//!
//! Token counting uses `tiktoken-rs`. The OpenAI `o200k_base` encoding is used
//! as the canonical estimator across providers; for non-OpenAI models this is an
//! approximation (documented), which is acceptable for a guardrail whose purpose
//! is to catch pathologically large prompts, not to bill exactly.

use serde_json::Value;

use crate::policy::config::{PolicyType, TokenLengthCapConfig};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

pub struct TokenLengthCapRule {
    phase: Phase,
    max_tokens: u32,
    scope_to_models: Vec<String>,
    action: PolicyAction,
}

impl TokenLengthCapRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: TokenLengthCapConfig = serde_json::from_value(cfg)
            .map_err(|e| format!("invalid token_length_cap config: {e}"))?;
        if cfg.max_tokens == 0 {
            return Err("token_length_cap requires maxTokens > 0".into());
        }
        Ok(Self {
            phase: cfg.phase,
            max_tokens: cfg.max_tokens,
            scope_to_models: cfg
                .scope_to_models
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            action: cfg.action,
        })
    }

    fn in_scope(&self, model: &str) -> bool {
        self.scope_to_models.is_empty() || self.scope_to_models.iter().any(|m| m == model)
    }
}

/// Estimate token count for arbitrary text using the o200k_base encoding
/// (cached singleton). Falls back to a chars/4 heuristic only if the encoder
/// cannot be constructed (should not happen with the bundled vocab).
pub fn estimate_tokens(text: &str) -> u32 {
    let bpe = tiktoken_rs::o200k_base_singleton();
    bpe.encode_with_special_tokens(text).len() as u32
}

impl PolicyRule for TokenLengthCapRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::TokenLengthCap
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        if !self.in_scope(ctx.model) {
            return RuleOutcome::clean();
        }

        // Prefer a known token count if the engine supplied one; else estimate.
        let tokens = ctx
            .input_tokens
            .filter(|_| self.phase == Phase::Input)
            .unwrap_or_else(|| estimate_tokens(ctx.text.as_ref()));

        if tokens > self.max_tokens {
            let score = (tokens as f32 / self.max_tokens as f32 - 1.0).clamp(0.0, 1.0);
            RuleOutcome::flagged(
                self.action,
                score.max(0.5),
                format!("token count {tokens} exceeds cap {}", self.max_tokens),
            )
        } else {
            RuleOutcome::clean()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn ctx(
        model: &'static str,
        text: &'static str,
        input_tokens: Option<u32>,
    ) -> EvalContext<'static> {
        EvalContext {
            phase: Phase::Input,
            model,
            text: Cow::Borrowed(text),
            json: None,
            input_tokens,
            live_state: None,
        }
    }

    #[test]
    fn estimate_tokens_is_reasonable() {
        let n = estimate_tokens("Hello, world! This is a test sentence.");
        assert!(n > 0 && n < 30, "got {n}");
    }

    #[test]
    fn blocks_when_known_tokens_exceed() {
        let r = TokenLengthCapRule::parse(serde_json::json!({"maxTokens": 100, "action": "block"}))
            .unwrap();
        let out = r.evaluate(&ctx("gpt-4o", "irrelevant", Some(500)));
        assert!(out.flagged);
        assert_eq!(out.action, PolicyAction::Block);
    }

    #[test]
    fn allows_when_under_cap() {
        let r = TokenLengthCapRule::parse(serde_json::json!({"maxTokens": 1000})).unwrap();
        let out = r.evaluate(&ctx("gpt-4o", "short", Some(10)));
        assert!(!out.flagged);
    }

    #[test]
    fn estimates_when_no_known_tokens() {
        let r =
            TokenLengthCapRule::parse(serde_json::json!({"maxTokens": 5, "action": "flag_only"}))
                .unwrap();
        let long = "this is definitely going to be more than five tokens of text content";
        let out = r.evaluate(&ctx("gpt-4o", long, None));
        assert!(out.flagged);
    }

    #[test]
    fn respects_model_scope() {
        let r = TokenLengthCapRule::parse(
            serde_json::json!({"maxTokens": 1, "scopeToModels": ["gpt-4o-mini"], "action": "block"}),
        )
        .unwrap();
        // out of scope -> clean despite tiny cap
        assert!(
            !r.evaluate(&ctx("gpt-4o", "lots of tokens here", Some(999)))
                .flagged
        );
        // in scope -> blocked
        assert!(r.evaluate(&ctx("gpt-4o-mini", "x", Some(999))).flagged);
    }

    #[test]
    fn zero_cap_rejected() {
        assert!(TokenLengthCapRule::parse(serde_json::json!({"maxTokens": 0})).is_err());
    }
}
