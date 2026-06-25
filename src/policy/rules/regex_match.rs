//! `regex_match` — match input/output text against a set of named regular
//! expressions, then block / redact / mask / replace / flag.
//!
//! Patterns are compiled once with a bounded program size. The `regex` crate is
//! linear-time by construction (finite automata, no backtracking), so untrusted
//! patterns cannot cause catastrophic ReDoS; the configured `regex_timeout_ms`
//! is retained for forward compatibility and surfaced in the compile error if a
//! pattern's compiled size is rejected.

use regex::{Regex, RegexBuilder};
use serde_json::Value;

use crate::policy::config::{PolicyType, RegexMatchConfig, RegexPattern};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

/// Cap on the compiled program size per pattern (1 MiB) — guards against an
/// untrusted pattern compiling to an enormous automaton.
const COMPILED_SIZE_LIMIT: usize = 1 << 20;

struct CompiledPattern {
    name: String,
    re: Regex,
}

pub struct RegexMatchRule {
    phase: Phase,
    patterns: Vec<CompiledPattern>,
    action: PolicyAction,
    redact_with: String,
}

impl RegexMatchRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: RegexMatchConfig =
            serde_json::from_value(cfg).map_err(|e| format!("invalid regex_match config: {e}"))?;
        if cfg.patterns.is_empty() {
            return Err("regex_match requires at least one pattern".into());
        }
        let mut patterns = Vec::with_capacity(cfg.patterns.len());
        for p in &cfg.patterns {
            patterns.push(CompiledPattern {
                name: p.name.clone(),
                re: compile_pattern(p)?,
            });
        }
        Ok(Self {
            phase: cfg.phase,
            patterns,
            action: cfg.action,
            redact_with: cfg.redact_with,
        })
    }
}

fn compile_pattern(p: &RegexPattern) -> Result<Regex, String> {
    let mut builder = RegexBuilder::new(&p.regex);
    builder.size_limit(COMPILED_SIZE_LIMIT);
    for flag in p.flags.chars() {
        match flag {
            'i' => {
                builder.case_insensitive(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            's' => {
                builder.dot_matches_new_line(true);
            }
            'x' => {
                builder.ignore_whitespace(true);
            }
            other => {
                return Err(format!(
                    "unsupported regex flag '{other}' in pattern '{}'",
                    p.name
                ))
            }
        }
    }
    builder.build().map_err(|e| {
        format!(
            "pattern '{}' failed to compile (possibly too large): {e}",
            p.name
        )
    })
}

impl PolicyRule for RegexMatchRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::RegexMatch
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let text = ctx.text.as_ref();
        let mut matched_names: Vec<String> = Vec::new();

        for p in &self.patterns {
            if p.re.is_match(text) {
                matched_names.push(p.name.clone());
            }
        }

        if matched_names.is_empty() {
            return RuleOutcome::clean();
        }

        let reason = format!("matched regex pattern(s): {}", matched_names.join(", "));
        let mut outcome =
            RuleOutcome::flagged(self.action, 1.0, reason).with_entities(matched_names);

        // For transform actions, rewrite the matched spans.
        if self.action.is_transform() {
            let mut transformed = text.to_string();
            for p in &self.patterns {
                let replacement: &str = match self.action {
                    PolicyAction::Redact | PolicyAction::Replace => self.redact_with.as_str(),
                    PolicyAction::Mask => "****", // fixed mask token
                    PolicyAction::Hash => "[HASHED]",
                    _ => self.redact_with.as_str(),
                };
                transformed = p.re.replace_all(&transformed, replacement).into_owned();
            }
            outcome = outcome.with_transformed(transformed);
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn ctx(text: &'static str) -> EvalContext<'static> {
        EvalContext {
            phase: Phase::Output,
            model: "gpt-4o",
            text: Cow::Borrowed(text),
            json: None,
            input_tokens: None,
            live_state: None,
        }
    }

    fn rule(json: serde_json::Value) -> RegexMatchRule {
        RegexMatchRule::parse(json).unwrap()
    }

    #[test]
    fn blocks_on_ssn_match() {
        let r = rule(serde_json::json!({
            "phase": "output",
            "patterns": [{"name": "ssn_us", "regex": "\\b\\d{3}-\\d{2}-\\d{4}\\b"}],
            "action": "block"
        }));
        let out = r.evaluate(&ctx("my ssn is 123-45-6789 ok"));
        assert!(out.flagged);
        assert_eq!(out.action, PolicyAction::Block);
        assert_eq!(out.matched_entities, vec!["ssn_us".to_string()]);
    }

    #[test]
    fn clean_when_no_match() {
        let r = rule(serde_json::json!({
            "patterns": [{"name": "ssn", "regex": "\\d{3}-\\d{2}-\\d{4}"}]
        }));
        assert!(!r.evaluate(&ctx("nothing sensitive here")).flagged);
    }

    #[test]
    fn redact_transforms_text() {
        let r = rule(serde_json::json!({
            "patterns": [{"name": "ssn", "regex": "\\d{3}-\\d{2}-\\d{4}"}],
            "action": "redact",
            "redactWith": "[SSN]"
        }));
        let out = r.evaluate(&ctx("ssn 123-45-6789 end"));
        assert!(out.flagged);
        assert_eq!(out.transformed_text.as_deref(), Some("ssn [SSN] end"));
    }

    #[test]
    fn mask_uses_fixed_token() {
        let r = rule(serde_json::json!({
            "patterns": [{"name": "ssn", "regex": "\\d{3}-\\d{2}-\\d{4}"}],
            "action": "mask"
        }));
        let out = r.evaluate(&ctx("123-45-6789"));
        assert_eq!(out.transformed_text.as_deref(), Some("****"));
    }

    #[test]
    fn case_insensitive_flag() {
        let r = rule(serde_json::json!({
            "patterns": [{"name": "secret", "regex": "topsecret", "flags": "i"}],
            "action": "flag_only"
        }));
        assert!(r.evaluate(&ctx("This is TOPSECRET info")).flagged);
    }

    #[test]
    fn multiple_patterns_collect_names() {
        let r = rule(serde_json::json!({
            "patterns": [
                {"name": "ssn", "regex": "\\d{3}-\\d{2}-\\d{4}"},
                {"name": "phone", "regex": "\\d{3}-\\d{4}"}
            ],
            "action": "flag_only"
        }));
        let out = r.evaluate(&ctx("123-45-6789"));
        assert!(out.flagged);
        // both patterns can match the SSN substring; at least ssn must be present
        assert!(out.matched_entities.contains(&"ssn".to_string()));
    }

    #[test]
    fn empty_patterns_rejected() {
        assert!(RegexMatchRule::parse(serde_json::json!({"patterns": []})).is_err());
    }

    #[test]
    fn invalid_regex_rejected() {
        let err = RegexMatchRule::parse(serde_json::json!({
            "patterns": [{"name": "bad", "regex": "("}]
        }));
        assert!(err.is_err());
    }

    #[test]
    fn unsupported_flag_rejected() {
        let err = RegexMatchRule::parse(serde_json::json!({
            "patterns": [{"name": "x", "regex": "a", "flags": "z"}]
        }));
        assert!(err.is_err());
    }

    #[test]
    fn linear_time_on_adversarial_input() {
        // A pattern that would catastrophically backtrack in a PCRE engine.
        // The `regex` crate runs this in linear time; assert it returns quickly
        // and correctly rather than hanging.
        let r = rule(serde_json::json!({
            "patterns": [{"name": "evil", "regex": "(a+)+$"}],
            "action": "flag_only"
        }));
        let input = "a".repeat(40) + "b"; // classic ReDoS trigger for backtrackers
        let start = std::time::Instant::now();
        let out = r.evaluate(&ctx(Box::leak(input.into_boxed_str())));
        assert!(
            start.elapsed().as_millis() < 100,
            "regex must be linear-time"
        );
        assert!(!out.flagged); // 'b' at end means no full match
    }
}
