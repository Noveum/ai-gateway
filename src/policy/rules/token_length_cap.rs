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

/// Estimate token count for arbitrary text.
///
/// Native builds use the exact `o200k_base` BPE encoding (cached singleton).
/// `tiktoken-rs` bundles a multi-megabyte vocab and pulls `fancy-regex`, which
/// would bloat the Worker bundle, so the wasm32 build cannot use it.
#[cfg(not(target_arch = "wasm32"))]
pub fn estimate_tokens(text: &str) -> u32 {
    let bpe = tiktoken_rs::o200k_base_singleton();
    bpe.encode_with_special_tokens(text).len() as u32
}

/// wasm32 / Cloudflare Worker token estimate: a SAFE UPPER BOUND.
///
/// `TokenLengthCapRule` fires only when the estimate *exceeds* `max_tokens`, so
/// the wasm estimator must never UNDER-count — otherwise an over-limit prompt
/// (CJK, emoji, minified code) slips past the cap. For byte-level BPE (o200k),
/// the token count is always ≤ the UTF-8 byte length (the base alphabet is
/// single bytes; merges only ever reduce the count), so `text.len()` is a sound
/// upper bound. It over-counts versus the exact native count, so the edge errs
/// toward blocking — the correct failure mode for a guardrail.
#[cfg(target_arch = "wasm32")]
pub fn estimate_tokens(text: &str) -> u32 {
    text.len().min(u32::MAX as usize) as u32
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
        // Bound the text we tokenize to ~8 chars per allowed token: that slice
        // always yields well over `max_tokens` tokens for real text, so if the
        // input is longer we can flag without tokenizing megabytes synchronously.
        let tokens = ctx
            .input_tokens
            .filter(|_| self.phase == Phase::Input)
            .unwrap_or_else(|| {
                let text = ctx.text.as_ref();
                let cap_chars = (self.max_tokens as usize)
                    .saturating_add(1)
                    .saturating_mul(8);
                let slice = if text.len() > cap_chars {
                    // Truncate on a char boundary at or before cap_chars.
                    let mut end = cap_chars.min(text.len());
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    &text[..end]
                } else {
                    text
                };
                estimate_tokens(slice)
            });

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
        let text = "Hello, world! This is a test sentence.";
        let n = estimate_tokens(text);
        // Native: exact BPE count (well under 30 for this short sentence).
        #[cfg(not(target_arch = "wasm32"))]
        assert!(n > 0 && n < 30, "got {n}");
        // wasm32: a safe UPPER bound == UTF-8 byte length (never undercounts).
        #[cfg(target_arch = "wasm32")]
        assert_eq!(n, text.len() as u32, "wasm estimate must equal byte length");
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
