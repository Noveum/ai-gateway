//! `pii_detection` — deterministic, regex-based PII detection.
//!
//! This is the in-process fast path covering the highest-impact entity types.
//! It deliberately mirrors the Nova Guard SDK's "regex fallback" set; full
//! Presidio-equivalent detection (NER-based) is delivered through the
//! `scorer_gate` path to the NovaEval service, identical to the SDK.
//!
//! On a match the policy masks (default), redacts, or blocks. Masking replaces
//! each matched span with the configured mask character repeated, preserving
//! approximate length so downstream formatting is less disturbed.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::policy::config::{PiiDetectionConfig, PolicyType};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

/// A built-in PII entity recognizer.
struct EntityPattern {
    /// Canonical entity name, e.g. `EMAIL_ADDRESS`.
    name: &'static str,
    re: Regex,
}

/// The curated built-in entity set. Patterns are intentionally conservative
/// (favor precision) to limit false positives in a blocking guardrail.
static ENTITIES: Lazy<Vec<EntityPattern>> = Lazy::new(|| {
    let defs: &[(&str, &str)] = &[
        (
            "EMAIL_ADDRESS",
            r"(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}\b",
        ),
        // US SSN: 3-2-4 with separators; avoids all-zero groups loosely.
        ("US_SSN", r"\b\d{3}-\d{2}-\d{4}\b"),
        // Credit card: 13-19 digits with optional single space/dash separators.
        // A Luhn check (applied at match time) filters out arbitrary numeric IDs.
        ("CREDIT_CARD_NUMBER", r"\b\d(?:[ -]?\d){12,18}\b"),
        // North American phone numbers (loose).
        (
            "PHONE_NUMBER",
            r"\b(?:\+?1[ .\-]?)?(?:\(?\d{3}\)?[ .\-]?)\d{3}[ .\-]?\d{4}\b",
        ),
        (
            "IP_ADDRESS",
            r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\b",
        ),
        ("IBAN_CODE", r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b"),
        // US passport: one letter + 8 digits (also matches some other IDs; precision-limited).
        ("US_PASSPORT", r"\b[A-Z]\d{8}\b"),
    ];
    defs.iter()
        .filter_map(|(name, pat)| Regex::new(pat).ok().map(|re| EntityPattern { name, re }))
        .collect()
});

pub struct PiiRule {
    phase: Phase,
    /// Active entity names (uppercased). Empty = all built-ins.
    entities: Vec<String>,
    action: PolicyAction,
    mask_char: String,
}

impl PiiRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: PiiDetectionConfig = serde_json::from_value(cfg)
            .map_err(|e| format!("invalid pii_detection config: {e}"))?;
        let entities: Vec<String> = cfg.entities.iter().map(|s| s.to_uppercase()).collect();
        // Validate that requested entities are known.
        for e in &entities {
            if !ENTITIES.iter().any(|p| p.name == e) {
                return Err(format!("unknown PII entity '{e}'"));
            }
        }
        Ok(Self {
            phase: cfg.phase,
            entities,
            action: cfg.action,
            mask_char: if cfg.mask_char.is_empty() {
                "*".to_string()
            } else {
                cfg.mask_char
            },
        })
    }

    fn is_active(&self, name: &str) -> bool {
        self.entities.is_empty() || self.entities.iter().any(|e| e == name)
    }
}

/// Luhn checksum over the digits in `s` (non-digits ignored). Requires at least
/// 13 digits. Used to keep `CREDIT_CARD_NUMBER` from matching arbitrary numeric
/// identifiers (phone numbers, order ids, etc.).
fn luhn_valid(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() < 13 {
        return false;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = *d;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 == 0
}

impl PolicyRule for PiiRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::PiiDetection
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let text = ctx.text.as_ref();
        let mut found: Vec<String> = Vec::new();
        let mut transformed = text.to_string();

        for entity in ENTITIES.iter() {
            if !self.is_active(entity.name) {
                continue;
            }
            let is_cc = entity.name == "CREDIT_CARD_NUMBER";

            // Detection: credit cards additionally require a valid Luhn checksum.
            let detected = if is_cc {
                entity.re.find_iter(text).any(|m| luhn_valid(m.as_str()))
            } else {
                entity.re.is_match(text)
            };
            if !detected {
                continue;
            }

            found.push(entity.name.to_string());
            if self.action.is_transform() {
                transformed = entity
                    .re
                    .replace_all(&transformed, |caps: &regex::Captures| {
                        let matched = &caps[0];
                        // For credit cards, leave non-Luhn-valid runs untouched.
                        if is_cc && !luhn_valid(matched) {
                            return matched.to_string();
                        }
                        match self.action {
                            PolicyAction::Mask => {
                                self.mask_char.repeat(matched.chars().count().min(16))
                            }
                            PolicyAction::Redact => "[REDACTED]".to_string(),
                            PolicyAction::Hash => "[HASHED]".to_string(),
                            PolicyAction::Replace => format!("[{}]", entity.name),
                            _ => matched.to_string(),
                        }
                    })
                    .into_owned();
            }
        }

        if found.is_empty() {
            return RuleOutcome::clean();
        }

        let reason = format!("detected PII entities: {}", found.join(", "));
        let mut outcome = RuleOutcome::flagged(self.action, 0.9, reason).with_entities(found);
        if self.action.is_transform() {
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
            phase: Phase::Both,
            model: "gpt-4o",
            text: Cow::Borrowed(text),
            json: None,
            input_tokens: None,
            live_state: None,
        }
    }

    #[test]
    fn detects_email() {
        let r = PiiRule::parse(
            serde_json::json!({"entities": ["EMAIL_ADDRESS"], "action": "flag_only"}),
        )
        .unwrap();
        let out = r.evaluate(&ctx("contact me at john.doe@acme.io please"));
        assert!(out.flagged);
        assert!(out.matched_entities.contains(&"EMAIL_ADDRESS".to_string()));
    }

    #[test]
    fn detects_ssn() {
        let r =
            PiiRule::parse(serde_json::json!({"entities": ["US_SSN"], "action": "block"})).unwrap();
        assert!(r.evaluate(&ctx("ssn 123-45-6789")).flagged);
    }

    #[test]
    fn luhn_helper() {
        assert!(luhn_valid("4242424242424242")); // Stripe test Visa
        assert!(luhn_valid("4111 1111 1111 1111")); // with spaces
        assert!(!luhn_valid("4242424242424243")); // bad checksum
        assert!(!luhn_valid("123456789012")); // too short
    }

    #[test]
    fn credit_card_requires_luhn() {
        let r = PiiRule::parse(
            serde_json::json!({"entities": ["CREDIT_CARD_NUMBER"], "action": "block"}),
        )
        .unwrap();
        // Valid Luhn card -> detected
        assert!(r.evaluate(&ctx("card 4242 4242 4242 4242 ok")).flagged);
        // A 16-digit non-card numeric id (fails Luhn) -> NOT flagged
        assert!(!r.evaluate(&ctx("order 1234567890123456 shipped")).flagged);
    }

    #[test]
    fn credit_card_mask_leaves_non_luhn_runs() {
        let r = PiiRule::parse(serde_json::json!({
            "entities": ["CREDIT_CARD_NUMBER"], "action": "redact"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("pay 4242424242424242 not 1111111111111111"));
        // valid card redacted; non-Luhn run preserved
        let t = out.transformed_text.unwrap();
        assert!(t.contains("[REDACTED]"));
        assert!(t.contains("1111111111111111"));
    }

    #[test]
    fn masks_email() {
        let r = PiiRule::parse(serde_json::json!({
            "entities": ["EMAIL_ADDRESS"], "action": "mask", "maskChar": "*"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("email a@b.com here"));
        assert!(out.transformed_text.is_some());
        assert!(!out.transformed_text.as_ref().unwrap().contains("a@b.com"));
    }

    #[test]
    fn replace_uses_entity_label() {
        let r = PiiRule::parse(serde_json::json!({
            "entities": ["EMAIL_ADDRESS"], "action": "replace"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("mail x@y.com"));
        assert_eq!(
            out.transformed_text.as_deref(),
            Some("mail [EMAIL_ADDRESS]")
        );
    }

    #[test]
    fn all_entities_when_unspecified() {
        let r = PiiRule::parse(serde_json::json!({"action": "flag_only"})).unwrap();
        let out = r.evaluate(&ctx("ip 192.168.1.1 and mail z@z.io"));
        assert!(out.matched_entities.len() >= 2);
    }

    #[test]
    fn clean_when_no_pii() {
        let r = PiiRule::parse(serde_json::json!({"action": "flag_only"})).unwrap();
        assert!(!r.evaluate(&ctx("just a normal sentence")).flagged);
    }

    #[test]
    fn unknown_entity_rejected() {
        assert!(PiiRule::parse(serde_json::json!({"entities": ["NOT_A_THING"]})).is_err());
    }
}
