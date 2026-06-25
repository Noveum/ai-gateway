//! `secrets_detection` — deterministic detection of leaked credentials.
//!
//! Regex-based detectors for the highest-impact secret classes (cloud keys,
//! VCS/CI tokens, provider keys, private keys, JWTs). Blocks by default. As with
//! PII, this is the in-process fast path; entropy-based and verification-based
//! detection (trufflehog-style) is delivered via the scorer-gate path.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::policy::config::{PolicyType, SecretsDetectionConfig};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

struct Detector {
    /// Detector id, e.g. `aws_access_key`.
    id: &'static str,
    re: Regex,
}

static DETECTORS: Lazy<Vec<Detector>> = Lazy::new(|| {
    let defs: &[(&str, &str)] = &[
        (
            "aws_access_key",
            r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA)[A-Z0-9]{16}\b",
        ),
        ("github_pat", r"\bghp_[A-Za-z0-9]{36}\b"),
        ("github_oauth", r"\bgho_[A-Za-z0-9]{36}\b"),
        ("gitlab_pat", r"\bglpat-[A-Za-z0-9\-_]{20}\b"),
        ("slack_token", r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b"),
        ("stripe_secret", r"\bsk_live_[A-Za-z0-9]{16,}\b"),
        ("openai_key", r"\bsk-[A-Za-z0-9]{20,}\b"),
        ("anthropic_key", r"\bsk-ant-[A-Za-z0-9\-_]{20,}\b"),
        ("google_api_key", r"\bAIza[0-9A-Za-z\-_]{35}\b"),
        (
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b",
        ),
        (
            "private_key_pem",
            r"-----BEGIN (?:RSA |EC |OPENSSH |PGP )?PRIVATE KEY-----",
        ),
        (
            "basic_auth_url",
            r"\b[a-z][a-z0-9+.\-]*://[^/\s:@]+:[^/\s:@]+@",
        ),
    ];
    defs.iter()
        .filter_map(|(id, pat)| Regex::new(pat).ok().map(|re| Detector { id, re }))
        .collect()
});

pub struct SecretsRule {
    phase: Phase,
    detectors: Vec<String>,
    action: PolicyAction,
}

impl SecretsRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: SecretsDetectionConfig = serde_json::from_value(cfg)
            .map_err(|e| format!("invalid secrets_detection config: {e}"))?;
        let detectors: Vec<String> = cfg.detectors.iter().map(|s| s.to_lowercase()).collect();
        for d in &detectors {
            if !DETECTORS.iter().any(|x| x.id == d) {
                return Err(format!("unknown secrets detector '{d}'"));
            }
        }
        Ok(Self {
            phase: cfg.phase,
            detectors,
            action: cfg.action,
        })
    }

    fn is_active(&self, id: &str) -> bool {
        self.detectors.is_empty() || self.detectors.iter().any(|d| d == id)
    }
}

impl PolicyRule for SecretsRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::SecretsDetection
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let text = ctx.text.as_ref();
        let mut found: Vec<String> = Vec::new();
        let mut transformed = text.to_string();

        for d in DETECTORS.iter() {
            if !self.is_active(d.id) {
                continue;
            }
            if d.re.is_match(text) {
                found.push(d.id.to_string());
                if self.action.is_transform() {
                    transformed =
                        d.re.replace_all(&transformed, "[REDACTED_SECRET]")
                            .into_owned();
                }
            }
        }

        if found.is_empty() {
            return RuleOutcome::clean();
        }

        // Leaked secrets are always maximum severity.
        let reason = format!("detected secret(s): {}", found.join(", "));
        let mut outcome = RuleOutcome::flagged(self.action, 1.0, reason).with_entities(found);
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
    fn detects_aws_key() {
        let r = SecretsRule::parse(
            serde_json::json!({"detectors": ["aws_access_key"], "action": "block"}),
        )
        .unwrap();
        let out = r.evaluate(&ctx("key is AKIAIOSFODNN7EXAMPLE here"));
        assert!(out.flagged);
        assert!(out.matched_entities.contains(&"aws_access_key".to_string()));
    }

    #[test]
    fn detects_github_pat() {
        let r = SecretsRule::parse(serde_json::json!({"action": "block"})).unwrap();
        let token = format!("ghp_{}", "a".repeat(36));
        assert!(
            r.evaluate(&ctx(Box::leak(format!("token {token}").into_boxed_str())))
                .flagged
        );
    }

    #[test]
    fn detects_private_key_header() {
        let r = SecretsRule::parse(serde_json::json!({"action": "block"})).unwrap();
        assert!(r.evaluate(&ctx("-----BEGIN RSA PRIVATE KEY-----")).flagged);
    }

    #[test]
    fn redacts_secret() {
        let r = SecretsRule::parse(serde_json::json!({
            "detectors": ["aws_access_key"], "action": "redact"
        }))
        .unwrap();
        let out = r.evaluate(&ctx("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(out.transformed_text.as_deref(), Some("[REDACTED_SECRET]"));
    }

    #[test]
    fn clean_when_no_secret() {
        let r = SecretsRule::parse(serde_json::json!({"action": "block"})).unwrap();
        assert!(!r.evaluate(&ctx("nothing secret here, just text")).flagged);
    }

    #[test]
    fn unknown_detector_rejected() {
        assert!(SecretsRule::parse(serde_json::json!({"detectors": ["nope"]})).is_err());
    }

    #[test]
    fn jwt_detected() {
        let r =
            SecretsRule::parse(serde_json::json!({"detectors": ["jwt"], "action": "flag_only"}))
                .unwrap();
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N";
        assert!(r.evaluate(&ctx(jwt)).flagged);
    }
}
