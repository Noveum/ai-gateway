//! `banned_substrings` — block/redact a fixed list of literal terms.
//!
//! Uses an Aho-Corasick automaton for linear-time matching of many literal
//! patterns in a single pass, far cheaper than a large regex alternation.

use aho_corasick::{AhoCorasick, MatchKind};
use serde_json::Value;

use crate::policy::config::{BannedSubstringsConfig, PolicyType};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

pub struct BannedSubstringsRule {
    phase: Phase,
    automaton: AhoCorasick,
    words: Vec<String>,
    action: PolicyAction,
    redact_with: String,
}

impl BannedSubstringsRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: BannedSubstringsConfig = serde_json::from_value(cfg)
            .map_err(|e| format!("invalid banned_substrings config: {e}"))?;
        if cfg.words.is_empty() {
            return Err("banned_substrings requires a non-empty `words` list".into());
        }
        let automaton = AhoCorasick::builder()
            .ascii_case_insensitive(cfg.case_insensitive)
            .match_kind(MatchKind::LeftmostLongest)
            .build(&cfg.words)
            .map_err(|e| format!("failed to build banned-substrings automaton: {e}"))?;
        Ok(Self {
            phase: cfg.phase,
            automaton,
            words: cfg.words,
            action: cfg.action,
            redact_with: cfg.redact_with,
        })
    }
}

impl PolicyRule for BannedSubstringsRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::BannedSubstrings
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let text = ctx.text.as_ref();

        let mut hit_patterns: Vec<usize> = Vec::new();
        for m in self.automaton.find_iter(text) {
            let pid = m.pattern().as_usize();
            if !hit_patterns.contains(&pid) {
                hit_patterns.push(pid);
            }
        }

        if hit_patterns.is_empty() {
            return RuleOutcome::clean();
        }

        let matched: Vec<String> = hit_patterns
            .iter()
            .filter_map(|&i| self.words.get(i).cloned())
            .collect();
        let reason = format!("contains banned term(s): {}", matched.join(", "));
        let mut outcome = RuleOutcome::flagged(self.action, 1.0, reason).with_entities(matched);

        if self.action.is_transform() {
            // Replace every banned term occurrence with the redaction token.
            let replaced = self
                .automaton
                .replace_all(text, &vec![self.redact_with.as_str(); self.words.len()]);
            outcome = outcome.with_transformed(replaced);
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
            phase: Phase::Both,
            model: "gpt-4o",
            text: Cow::Borrowed(text),
            json: None,
            input_tokens: None,
            live_state: None,
        }
    }

    #[test]
    fn blocks_on_banned_word() {
        let r = BannedSubstringsRule::parse(serde_json::json!({
            "words": ["project_atlas", "competitor_x"],
            "action": "block"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("we discussed project_atlas today"));
        assert!(out.flagged);
        assert!(out.matched_entities.contains(&"project_atlas".to_string()));
    }

    #[test]
    fn case_insensitive_default() {
        let r = BannedSubstringsRule::parse(serde_json::json!({"words": ["Secret"]})).unwrap();
        assert!(r.evaluate(&ctx("this is SECRET")).flagged);
    }

    #[test]
    fn case_sensitive_when_disabled() {
        let r = BannedSubstringsRule::parse(
            serde_json::json!({"words": ["Secret"], "caseInsensitive": false}),
        )
        .unwrap();
        assert!(!r.evaluate(&ctx("this is secret")).flagged);
        assert!(r.evaluate(&ctx("this is Secret")).flagged);
    }

    #[test]
    fn redact_replaces_all_occurrences() {
        let r = BannedSubstringsRule::parse(serde_json::json!({
            "words": ["badword"],
            "action": "redact",
            "redactWith": "[X]"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("badword and badword again"));
        assert_eq!(out.transformed_text.as_deref(), Some("[X] and [X] again"));
    }

    #[test]
    fn clean_when_absent() {
        let r = BannedSubstringsRule::parse(serde_json::json!({"words": ["nope"]})).unwrap();
        assert!(!r.evaluate(&ctx("totally fine text")).flagged);
    }

    #[test]
    fn empty_words_rejected() {
        assert!(BannedSubstringsRule::parse(serde_json::json!({"words": []})).is_err());
    }

    #[test]
    fn multiple_distinct_terms_collected() {
        let r = BannedSubstringsRule::parse(serde_json::json!({
            "words": ["alpha", "beta"], "action": "flag_only"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("alpha then beta then alpha"));
        assert_eq!(out.matched_entities.len(), 2);
    }
}
