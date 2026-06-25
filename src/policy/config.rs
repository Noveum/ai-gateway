//! Policy bundle schema — the `nova-guard.json` format shared with the Nova Guard
//! SDK and control plane.
//!
//! A [`PolicyBundle`] is a portable, version-controllable list of [`Policy`]
//! definitions. Each policy carries common fields (name, type, mode, priority,
//! fail-closed) plus a type-specific `config` object. The per-type config structs
//! defined here deserialize from that `config` object; unknown or future policy
//! types deserialize into [`PolicyType::Unknown`] and are skipped by the engine
//! with a warning rather than failing the whole bundle.

use serde::{Deserialize, Serialize};

use super::decision::{Phase, PolicyAction, PolicyMode};

/// The canonical bundle envelope (`nova-guard.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyBundle {
    #[serde(default)]
    pub bundle_version: Option<String>,
    #[serde(default)]
    pub exported_at: Option<String>,
    #[serde(default)]
    pub scope: Option<BundleScope>,
    #[serde(default)]
    pub policies: Vec<Policy>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleScope {
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

impl PolicyBundle {
    /// Parse a bundle from JSON bytes.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Parse a bundle from a JSON string.
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// One policy definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    #[serde(default)]
    pub policy_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub policy_type: PolicyType,
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default)]
    pub fail_closed: bool,
    #[serde(default = "default_strictness")]
    pub strictness: u8,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Type-specific configuration, parsed by the matching rule.
    #[serde(default)]
    pub config: serde_json::Value,
}

fn default_strictness() -> u8 {
    2
}
fn default_priority() -> i32 {
    100
}
fn default_enabled() -> bool {
    true
}

impl Policy {
    /// A stable identifier for telemetry: the explicit id, else the name.
    pub fn id(&self) -> &str {
        self.policy_id.as_deref().unwrap_or(&self.name)
    }

    /// Should the engine consider this policy at all?
    pub fn is_active(&self) -> bool {
        self.enabled && self.mode != PolicyMode::Off
    }
}

/// The set of policy types. Deterministic types are enforced in-process by the
/// gateway; `ScorerGate` and the v1.5 classifier types are evaluated by calling
/// the NovaEval scoring service (shared with the SDK); unrecognized types
/// deserialize to `Unknown` and are skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyType {
    // --- v1 deterministic (enforced in-process) ---
    CostCap,
    RateLimit,
    ModelAllowlist,
    RegexMatch,
    BannedSubstrings,
    PiiDetection,
    SecretsDetection,
    JsonSchema,
    TokenLengthCap,
    // --- v1.5 (evaluated via NovaEval scorer-gate; recognized but routed out) ---
    ScorerGate,
    PromptInjection,
    TopicRestriction,
    ContentModeration,
    GroundingCheck,
    /// Any type this build does not recognize. Skipped with a warning.
    #[serde(other)]
    Unknown,
}

impl PolicyType {
    /// The wire/string form, matching the Nova Guard SDK and CRUD API.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyType::CostCap => "cost_cap",
            PolicyType::RateLimit => "rate_limit",
            PolicyType::ModelAllowlist => "model_allowlist",
            PolicyType::RegexMatch => "regex_match",
            PolicyType::BannedSubstrings => "banned_substrings",
            PolicyType::PiiDetection => "pii_detection",
            PolicyType::SecretsDetection => "secrets_detection",
            PolicyType::JsonSchema => "json_schema",
            PolicyType::TokenLengthCap => "token_length_cap",
            PolicyType::ScorerGate => "scorer_gate",
            PolicyType::PromptInjection => "prompt_injection",
            PolicyType::TopicRestriction => "topic_restriction",
            PolicyType::ContentModeration => "content_moderation",
            PolicyType::GroundingCheck => "grounding_check",
            PolicyType::Unknown => "unknown",
        }
    }

    /// Is this a deterministic type the gateway enforces locally (no network)?
    pub fn is_deterministic(self) -> bool {
        matches!(
            self,
            PolicyType::CostCap
                | PolicyType::RateLimit
                | PolicyType::ModelAllowlist
                | PolicyType::RegexMatch
                | PolicyType::BannedSubstrings
                | PolicyType::PiiDetection
                | PolicyType::SecretsDetection
                | PolicyType::JsonSchema
                | PolicyType::TokenLengthCap
        )
    }
}

// ---------------------------------------------------------------------------
// Per-type config structs (deserialized from `Policy.config`).
// ---------------------------------------------------------------------------

fn default_phase_input() -> Phase {
    Phase::Input
}
fn default_phase_output() -> Phase {
    Phase::Output
}
fn default_phase_both() -> Phase {
    Phase::Both
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegexPattern {
    pub name: String,
    pub regex: String,
    #[serde(default)]
    pub flags: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegexMatchConfig {
    #[serde(default = "default_phase_output")]
    pub phase: Phase,
    #[serde(default)]
    pub patterns: Vec<RegexPattern>,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
    #[serde(default = "default_redact_with")]
    pub redact_with: String,
    #[serde(default = "default_regex_timeout_ms")]
    pub regex_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedSubstringsConfig {
    #[serde(default = "default_phase_both")]
    pub phase: Phase,
    #[serde(default)]
    pub words: Vec<String>,
    #[serde(default = "default_true")]
    pub case_insensitive: bool,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
    #[serde(default = "default_redact_with")]
    pub redact_with: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAllowlistConfig {
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub denied: Vec<String>,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLengthCapConfig {
    #[serde(default = "default_phase_input")]
    pub phase: Phase,
    pub max_tokens: u32,
    #[serde(default)]
    pub scope_to_models: Vec<String>,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonSchemaConfig {
    #[serde(default = "default_phase_output")]
    pub phase: Phase,
    pub schema: serde_json::Value,
    #[serde(default = "default_true")]
    pub allow_markdown_code_fences: bool,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
}

/// Time window for a rolling/calendar cost cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostWindow {
    #[serde(rename = "1d_rolling")]
    OneDayRolling,
    #[serde(rename = "7d_rolling")]
    SevenDayRolling,
    #[serde(rename = "30d_rolling")]
    ThirtyDayRolling,
    #[serde(rename = "1d_calendar")]
    OneDayCalendar,
    #[serde(rename = "1mo_calendar")]
    OneMonthCalendar,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostCapConfig {
    pub window: CostWindow,
    pub max_usd: f64,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
    #[serde(default)]
    pub soft_usd: Option<f64>,
    #[serde(default)]
    pub scope_to_models: Option<Vec<String>>,
    /// `strict` uses the control-plane atomic reservation; `advisory` uses the
    /// cached live-state counter (no reservation hop).
    #[serde(default)]
    pub enforcement_mode: CostEnforcementMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CostEnforcementMode {
    #[default]
    Advisory,
    Strict,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateWindow {
    /// e.g. `"1m"`, `"1h"`, `"1d"`.
    pub period: String,
    #[serde(default)]
    pub max_requests: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitConfig {
    #[serde(default)]
    pub windows: Vec<RateWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiiDetectionConfig {
    #[serde(default = "default_phase_both")]
    pub phase: Phase,
    /// Entity types to detect, e.g. `["EMAIL_ADDRESS", "US_SSN"]`. Empty = all known.
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default = "mask_action")]
    pub action: PolicyAction,
    #[serde(default = "default_mask_char")]
    pub mask_char: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsDetectionConfig {
    #[serde(default = "default_phase_both")]
    pub phase: Phase,
    /// Detector ids, e.g. `["aws_access_key", "openai_sk"]`. Empty = all known.
    #[serde(default)]
    pub detectors: Vec<String>,
    #[serde(default = "block_action")]
    pub action: PolicyAction,
}

fn default_true() -> bool {
    true
}
fn block_action() -> PolicyAction {
    PolicyAction::Block
}
fn mask_action() -> PolicyAction {
    PolicyAction::Mask
}
fn default_redact_with() -> String {
    "[REDACTED]".to_string()
}
fn default_mask_char() -> String {
    "*".to_string()
}
fn default_regex_timeout_ms() -> u64 {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_bundle() {
        let json = r#"{
            "bundleVersion": "1.0",
            "policies": [
                {"name": "Block SSN", "type": "regex_match", "mode": "enforce",
                 "config": {"phase": "output", "patterns": [{"name": "ssn", "regex": "\\d{3}-\\d{2}-\\d{4}"}], "action": "block"}}
            ]
        }"#;
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        assert_eq!(bundle.policies.len(), 1);
        let p = &bundle.policies[0];
        assert_eq!(p.policy_type, PolicyType::RegexMatch);
        assert_eq!(p.mode, PolicyMode::Enforce);
        assert!(p.is_active());
        assert_eq!(p.id(), "Block SSN");
    }

    #[test]
    fn unknown_type_does_not_fail_bundle() {
        let json = r#"{"policies": [
            {"name": "future", "type": "quantum_entanglement_check", "config": {}}
        ]}"#;
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        assert_eq!(bundle.policies[0].policy_type, PolicyType::Unknown);
        assert!(!PolicyType::Unknown.is_deterministic());
    }

    #[test]
    fn defaults_apply() {
        let json = r#"{"policies": [{"name": "p", "type": "model_allowlist", "config": {"allowed": ["gpt-4o"]}}]}"#;
        let bundle = PolicyBundle::from_json_str(json).unwrap();
        let p = &bundle.policies[0];
        assert_eq!(p.mode, PolicyMode::Shadow); // default
        assert_eq!(p.strictness, 2);
        assert_eq!(p.priority, 100);
        assert!(p.enabled);
        assert!(!p.fail_closed);
    }

    #[test]
    fn policy_type_roundtrips_to_str() {
        for (json_name, ty) in [
            ("cost_cap", PolicyType::CostCap),
            ("regex_match", PolicyType::RegexMatch),
            ("banned_substrings", PolicyType::BannedSubstrings),
            ("pii_detection", PolicyType::PiiDetection),
            ("json_schema", PolicyType::JsonSchema),
            ("token_length_cap", PolicyType::TokenLengthCap),
            ("scorer_gate", PolicyType::ScorerGate),
        ] {
            assert_eq!(ty.as_str(), json_name);
            let parsed: PolicyType = serde_json::from_str(&format!("\"{json_name}\"")).unwrap();
            assert_eq!(parsed, ty);
        }
    }

    #[test]
    fn cost_window_aliases_parse() {
        let cfg: CostCapConfig = serde_json::from_value(serde_json::json!({
            "window": "1mo_calendar", "maxUsd": 1500.0, "action": "block"
        }))
        .unwrap();
        assert_eq!(cfg.window, CostWindow::OneMonthCalendar);
        assert_eq!(cfg.max_usd, 1500.0);
        assert_eq!(cfg.enforcement_mode, CostEnforcementMode::Advisory);
    }

    #[test]
    fn off_and_disabled_are_inactive() {
        let off: Policy = serde_json::from_value(serde_json::json!({
            "name": "p", "type": "regex_match", "mode": "off", "config": {}
        }))
        .unwrap();
        assert!(!off.is_active());

        let disabled: Policy = serde_json::from_value(serde_json::json!({
            "name": "p", "type": "regex_match", "enabled": false, "config": {}
        }))
        .unwrap();
        assert!(!disabled.is_active());
    }
}
