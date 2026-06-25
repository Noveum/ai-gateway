//! `json_schema` — validate that model output is JSON conforming to a schema.
//!
//! Typically an output-phase policy: the completion text is parsed as JSON
//! (optionally stripping a leading/trailing Markdown code fence) and validated
//! against a compiled JSON Schema. On failure the policy blocks (or flags).

use jsonschema::Validator;
use serde_json::Value;

use crate::policy::config::{JsonSchemaConfig, PolicyType};
use crate::policy::decision::{Phase, PolicyAction};

use super::{EvalContext, PolicyRule, RuleOutcome};

pub struct JsonSchemaRule {
    phase: Phase,
    validator: Validator,
    allow_fences: bool,
    action: PolicyAction,
}

impl JsonSchemaRule {
    pub fn parse(cfg: Value) -> Result<Self, String> {
        let cfg: JsonSchemaConfig =
            serde_json::from_value(cfg).map_err(|e| format!("invalid json_schema config: {e}"))?;
        let validator = jsonschema::validator_for(&cfg.schema)
            .map_err(|e| format!("invalid JSON Schema document: {e}"))?;
        Ok(Self {
            phase: cfg.phase,
            validator,
            allow_fences: cfg.allow_markdown_code_fences,
            action: cfg.action,
        })
    }
}

/// Strip a single leading ```/```json fence and trailing ``` if present.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // drop an optional language tag on the first line
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or("");
        rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        t
    }
}

impl PolicyRule for JsonSchemaRule {
    fn policy_type(&self) -> PolicyType {
        PolicyType::JsonSchema
    }

    fn phase(&self) -> Phase {
        self.phase
    }

    fn evaluate(&self, ctx: &EvalContext) -> RuleOutcome {
        let raw = ctx.text.as_ref();
        let candidate = if self.allow_fences {
            strip_code_fence(raw)
        } else {
            raw.trim()
        };

        let parsed: Value = match serde_json::from_str(candidate) {
            Ok(v) => v,
            Err(e) => {
                return RuleOutcome::flagged(
                    self.action,
                    1.0,
                    format!("output is not valid JSON: {e}"),
                )
                .with_entities(vec!["invalid_json".to_string()]);
            }
        };

        if self.validator.is_valid(&parsed) {
            RuleOutcome::clean()
        } else {
            let errors: Vec<String> = self
                .validator
                .iter_errors(&parsed)
                .map(|e| format!("{e}"))
                .take(5)
                .collect();
            RuleOutcome::flagged(
                self.action,
                1.0,
                format!("output failed schema validation: {}", errors.join("; ")),
            )
            .with_entities(vec!["schema_violation".to_string()])
        }
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

    fn rule(extra: serde_json::Value) -> JsonSchemaRule {
        let mut base = serde_json::json!({
            "schema": {
                "type": "object",
                "required": ["status"],
                "properties": {"status": {"type": "string", "enum": ["ok", "error"]}}
            },
            "action": "block"
        });
        if let Value::Object(m) = extra {
            for (k, v) in m {
                base[k] = v;
            }
        }
        JsonSchemaRule::parse(base).unwrap()
    }

    #[test]
    fn valid_json_passes() {
        let r = rule(serde_json::json!({}));
        assert!(!r.evaluate(&ctx(r#"{"status": "ok"}"#)).flagged);
    }

    #[test]
    fn schema_violation_blocks() {
        let r = rule(serde_json::json!({}));
        let out = r.evaluate(&ctx(r#"{"status": "maybe"}"#));
        assert!(out.flagged);
        assert_eq!(out.matched_entities, vec!["schema_violation".to_string()]);
    }

    #[test]
    fn missing_required_field_blocks() {
        let r = rule(serde_json::json!({}));
        assert!(r.evaluate(&ctx(r#"{"other": 1}"#)).flagged);
    }

    #[test]
    fn invalid_json_blocks() {
        let r = rule(serde_json::json!({}));
        let out = r.evaluate(&ctx("not json at all"));
        assert!(out.flagged);
        assert_eq!(out.matched_entities, vec!["invalid_json".to_string()]);
    }

    #[test]
    fn strips_markdown_fence() {
        let r = rule(serde_json::json!({"allowMarkdownCodeFences": true}));
        let fenced = "```json\n{\"status\": \"ok\"}\n```";
        assert!(!r.evaluate(&ctx(fenced)).flagged);
    }

    #[test]
    fn fence_not_stripped_when_disabled() {
        let r = rule(serde_json::json!({"allowMarkdownCodeFences": false}));
        let fenced = "```json\n{\"status\": \"ok\"}\n```";
        assert!(r.evaluate(&ctx(fenced)).flagged); // fence makes it invalid JSON
    }

    #[test]
    fn invalid_schema_document_rejected() {
        let bad = JsonSchemaRule::parse(serde_json::json!({"schema": {"type": "not_a_real_type"}}));
        assert!(bad.is_err());
    }
}
