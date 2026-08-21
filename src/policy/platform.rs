//! Bridge to the Noveum platform's NovaGuard API.
//!
//! Translates the platform's policy + live-state JSON (from
//! `GET /api/v1/projects/{id}/policies` and `.../policies/state`) into the
//! gateway's own [`PolicyBundle`](crate::policy::config::PolicyBundle) and
//! [`LiveState`](crate::policy::rules::LiveState). This is the pure, shared
//! mapping layer; the native HTTP fetch lives in [`crate::policy::remote`].
//!
//! Platform policy modes are `OFF`/`SHADOW`/`ENFORCE`, and actions use
//! SCREAMING_SNAKE_CASE. The shapes are deliberately close
//! to the gateway's: window keys (`1d_rolling`, …) and rate config field names
//! (`period`/`maxRequests`/`maxTokens`) already match; only the enum *casing*
//! (`ENFORCE`→`enforce`, `BLOCK`→`block`) differs.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::policy::config::{PolicyBundle, PolicyType};
use crate::policy::rules::LiveState;

/// The env vars/secrets that configure the platform bridge. Declared in this
/// shared module because both the native bootstrap
/// ([`crate::policy::remote::RemoteConfig::from_values`]) and the Worker's
/// refusal path validate the same pair, and their messages must agree.
pub const API_KEY_VAR: &str = "NOVEUM_API_KEY";
pub const PROJECT_ID_VAR: &str = "NOVEUM_GUARD_PROJECT_ID";
/// Selects the deployment mode (`dedicated` / `shared`). Declared here for the
/// same reason as the pair above: the native bootstrap *implements* both modes
/// and the Worker *refuses* the shared one, and neither may drift from the
/// other's spelling of the variable.
pub const TENANCY_VAR: &str = "NOVEUM_GUARD_TENANCY";

fn map_action(action: &str) -> String {
    // Preserve the action's semantics. In particular, FLAG_ONLY must never be
    // promoted to BLOCK: doing so turns an observe-only policy into an outage.
    // Unknown future values stay unknown after casing normalization, so bundle
    // parsing rejects them instead of silently granting them BLOCK semantics.
    action.trim().to_ascii_lowercase()
}

/// Lowercase the `action` enum(s) inside a platform policy config so it parses as
/// the gateway's `PolicyAction`. All other fields already line up.
fn normalize_config(policy_type: &str, mut config: Value) -> Value {
    if config.get("action").and_then(|a| a.as_str()).is_some() {
        let a = config["action"].as_str().unwrap();
        config["action"] = json!(map_action(a));
    }
    if policy_type == "cost_cap" {
        // `enforcementMode` selects platform-atomic admission (`strict`) over the
        // per-process pending ledger (`advisory`). The platform spells its enums
        // in SCREAMING_CASE; the gateway's `CostEnforcementMode` is snake_case,
        // and an unmatched variant is a *parse error* that would drop the whole
        // policy — i.e. silently remove a cap. Normalize the casing here.
        if let Some(m) = config.get("enforcementMode").and_then(|m| m.as_str()) {
            config["enforcementMode"] = json!(m.trim().to_ascii_lowercase());
        }
    }
    if policy_type == "rate_limit" {
        if let Some(windows) = config.get_mut("windows").and_then(|w| w.as_array_mut()) {
            for w in windows.iter_mut() {
                if let Some(a) = w.get("action").and_then(|a| a.as_str()) {
                    w["action"] = json!(map_action(a));
                }
            }
        }
    }
    config
}

/// Translate the platform `GET .../policies` payload into a gateway PolicyBundle
/// JSON object (`{"policies":[...]}`). Accepts an array, `{policies:[...]}`, or
/// `{data:[...]}`. Disabled policies and non-Phase-0 types are skipped.
pub fn translate_policies(platform: &Value) -> Value {
    let list = platform
        .get("policies")
        .and_then(|v| v.as_array())
        .or_else(|| platform.get("data").and_then(|v| v.as_array()))
        .or_else(|| platform.as_array());

    let mut policies = Vec::new();
    if let Some(items) = list {
        for p in items {
            if p.get("enabled").and_then(|e| e.as_bool()) == Some(false) {
                continue;
            }
            // Every type in `schema/novaguard-policy.v1.json` is translated,
            // not just the two the platform originally shipped. Dropping the
            // rest here was a silent third definition of "what a policy is":
            // an operator could create a policy in the UI that this bridge
            // deleted on the way in, with nothing anywhere saying so. Types the
            // engine cannot enforce are now carried through and rejected
            // loudly by `PolicyEngine::compile` instead.
            let raw_type = p.get("type").and_then(|t| t.as_str()).unwrap_or_default();
            let kind = PolicyType::from_platform_enum(raw_type);
            let policy_type = if kind == PolicyType::Unknown {
                // Keep the raw string so the engine's rejection can name it.
                raw_type
            } else {
                kind.as_str()
            };
            let name = p
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or(policy_type)
                .to_string();
            // The platform's `mode` field is deprecated — the UI no longer sets it,
            // so it serializes as `null`, and backend enforcement keys off `enabled`
            // (already checked above). So an enabled policy *enforces* unless it
            // carries an explicit SHADOW/OFF mode. Defaulting a mode-less policy to
            // `shadow` (the previous behavior) silently turned every platform policy
            // into a no-op that recorded decisions but never blocked.
            let mode = match p
                .get("mode")
                .and_then(|m| m.as_str())
                .map(|m| m.to_ascii_uppercase())
                .as_deref()
            {
                Some("OFF") => continue, // explicitly disabled → don't enforce at all
                Some("SHADOW") => "shadow",
                _ => "enforce", // "ENFORCE", or the common null/absent case
            };
            // Carry the platform's `failClosed` through so `cost_cap`/`rate_limit`
            // block (rather than fail open) when `/state` is unreachable.
            let fail_closed = p
                .get("failClosed")
                .and_then(|f| f.as_bool())
                .unwrap_or(false);
            // Carry the platform `policyId` so decisions + BLOCKED usage events
            // reference the real id (not the policy name).
            let policy_id = p.get("policyId").and_then(|v| v.as_str());
            // Carry `priority` so the engine evaluates policies in the platform's
            // order (lower number = higher precedence); otherwise every policy
            // collapses to the default priority and ordering is lost.
            let priority = p.get("priority").and_then(|v| v.as_i64());
            // Carry `source` (`project`/`org`) so org-wide caps can be evaluated
            // against org-scope counters instead of each project's own spend.
            let source = p.get("source").and_then(|v| v.as_str());
            let config =
                normalize_config(policy_type, p.get("config").cloned().unwrap_or(json!({})));
            let mut out = json!({
                "name": name,
                "type": policy_type,
                "mode": mode,
                "failClosed": fail_closed,
                "config": config,
            });
            if let Some(id) = policy_id {
                out["policyId"] = json!(id);
            }
            if let Some(pr) = priority {
                out["priority"] = json!(pr);
            }
            if let Some(s) = source {
                out["source"] = json!(s);
            }
            policies.push(out);
        }
    }

    json!({ "policies": policies })
}

/// Translate + parse the platform policies into a [`PolicyBundle`].
pub fn translate_bundle(platform: &Value) -> Result<PolicyBundle, String> {
    let bundle_json = translate_policies(platform);
    let s = serde_json::to_string(&bundle_json).map_err(|e| e.to_string())?;
    PolicyBundle::from_json_str(&s).map_err(|e| e.to_string())
}

/// Translate the platform `GET .../policies/state` payload into a [`LiveState`].
///
/// Platform shape: `{cost:{"1d_rolling":n,...,perModel:{}}, rate:{"requests_1m":n,
/// "tokens_1h":n,...}}`. The gateway keys cost by window (`1d_rolling`) and rate
/// by bare period (`1m`), which is what `eval_cost_cap`/`eval_rate_limit` expect.
pub fn state_to_live_state(state: &Value) -> LiveState {
    fn parse_scope(
        scope: &Value,
    ) -> (
        HashMap<String, f64>,
        HashMap<String, u64>,
        HashMap<String, u64>,
    ) {
        let mut cost_usd_by_window = HashMap::new();
        if let Some(cost) = scope.get("cost").and_then(|c| c.as_object()) {
            for (window, v) in cost {
                if window == "perModel" {
                    continue;
                }
                if let Some(n) = v.as_f64() {
                    cost_usd_by_window.insert(window.clone(), n);
                }
            }
        }

        let mut requests_by_window = HashMap::new();
        let mut tokens_by_window = HashMap::new();
        if let Some(rate) = scope.get("rate").and_then(|r| r.as_object()) {
            for (key, v) in rate {
                let n = v.as_u64().unwrap_or(0);
                if let Some(period) = key.strip_prefix("requests_") {
                    requests_by_window.insert(period.to_string(), n);
                } else if let Some(period) = key.strip_prefix("tokens_") {
                    tokens_by_window.insert(period.to_string(), n);
                }
            }
        }
        (cost_usd_by_window, requests_by_window, tokens_by_window)
    }

    let (cost_usd_by_window, requests_by_window, tokens_by_window) = parse_scope(state);
    // Optional org-scope aggregates (`{"org": {"cost": {...}, "rate": {...}}}`) —
    // used to evaluate org-sourced policies against org-wide spend. A platform
    // that doesn't ship them yet leaves these maps empty, which the engine
    // treats as *unavailable state* for an org policy (fail-closed blocks,
    // fail-open allows with a reason). It never substitutes project counters:
    // that would let every project consume the whole org allowance separately.
    let (org_cost_usd_by_window, org_requests_by_window, org_tokens_by_window) =
        state.get("org").map(parse_scope).unwrap_or_default();

    LiveState {
        cost_usd_by_window,
        requests_by_window,
        tokens_by_window,
        org_cost_usd_by_window,
        org_requests_by_window,
        org_tokens_by_window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::decision::Phase;
    use crate::policy::engine::{EngineOptions, PolicyEngine};

    #[test]
    fn translates_cost_cap_and_rate_limit_with_casing() {
        let platform = json!({"policies": [
            {"policyId":"p1","name":"Monthly budget","type":"COST_CAP","mode":"ENFORCE","enabled":true,"failClosed":true,"priority":10,
             "config":{"window":"30d_rolling","maxUsd":100.0,"softUsd":80.0,"action":"BLOCK"}},
            {"policyId":"p2","name":"Rate","type":"RATE_LIMIT","mode":"SHADOW","enabled":true,
             "config":{"windows":[{"period":"1m","maxRequests":60,"action":"BLOCK"}]}},
            {"policyId":"p3","name":"disabled","type":"COST_CAP","mode":"ENFORCE","enabled":false,
             "config":{"window":"1d_rolling","maxUsd":1.0,"action":"BLOCK"}},
            {"policyId":"p4","name":"unknown","type":"PROMPT_INJECTION","mode":"ENFORCE","enabled":true,"config":{}}
        ]});
        let out = translate_policies(&platform);
        let pols = out["policies"].as_array().unwrap();
        // Disabled policies are still skipped, but a type the gateway cannot
        // enforce is NO LONGER dropped here: it is carried through so the engine
        // can reject it by name. Dropping it made an operator's policy vanish
        // with nothing anywhere reporting it.
        assert_eq!(pols.len(), 3, "only the disabled policy is skipped");
        assert_eq!(pols[2]["type"], "prompt_injection");
        assert_eq!(pols[0]["type"], "cost_cap");
        assert_eq!(pols[0]["mode"], "enforce");
        assert_eq!(pols[0]["policyId"], "p1", "policyId carried through");
        assert_eq!(pols[0]["priority"], 10, "priority carried through");
        assert_eq!(pols[0]["failClosed"], true, "failClosed carried through");
        assert_eq!(pols[0]["config"]["action"], "block");
        assert_eq!(pols[0]["config"]["window"], "30d_rolling");
        assert_eq!(pols[0]["config"]["maxUsd"], 100.0);
        assert_eq!(pols[1]["type"], "rate_limit");
        assert_eq!(pols[1]["mode"], "shadow");
        assert_eq!(pols[1]["config"]["windows"][0]["action"], "block");
        // The translated bundle must parse + compile in the real engine. The
        // two enforceable policies compile; the reserved classifier type is
        // reported as a rejection rather than silently disappearing.
        let bundle = translate_bundle(&platform).expect("bundle parses");
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());
        assert_eq!(engine.active_policy_count(), 2);
        let rejected = engine.rejected_policies();
        assert_eq!(rejected.len(), 1, "{rejected:?}");
        assert!(rejected[0].contains("prompt_injection"), "{rejected:?}");
    }

    #[test]
    fn preserves_every_platform_action_instead_of_promoting_it_to_block() {
        for (platform_action, gateway_action) in [
            ("ALLOW", "allow"),
            ("BLOCK", "block"),
            ("REDACT", "redact"),
            ("MASK", "mask"),
            ("HASH", "hash"),
            ("REPLACE", "replace"),
            ("FLAG_ONLY", "flag_only"),
        ] {
            assert_eq!(
                map_action(platform_action),
                gateway_action,
                "platform action {platform_action} changed semantics during translation"
            );
        }
    }

    #[test]
    fn unsupported_flag_only_cost_cap_is_rejected_instead_of_promoted_to_block() {
        let platform = json!({"policies": [{
            "policyId": "p",
            "name": "invalid observe-only cap",
            "type": "COST_CAP",
            "mode": "ENFORCE",
            "enabled": true,
            "config": {
                "window": "1d_rolling",
                "maxUsd": 10.0,
                "action": "FLAG_ONLY",
                "enforcementMode": "STRICT"
            }
        }]});
        let bundle =
            translate_bundle(&platform).expect("the outer policy bundle remains parseable");
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());

        assert_eq!(engine.active_policy_count(), 0);
        let rejected = engine.rejected_policies();
        assert_eq!(rejected.len(), 1, "{rejected:?}");
        assert!(rejected[0].contains("invalid config"), "{rejected:?}");
    }

    #[test]
    fn every_contract_type_survives_translation() {
        // Before NOV-107 this bridge hard-coded COST_CAP + RATE_LIMIT, so the
        // other twelve types were unreachable from the product no matter what
        // the platform stored. Each one must now arrive at the engine.
        for ty in PolicyType::ALL {
            let platform = json!({"policies": [{
                "policyId": "p", "name": "p", "type": ty.platform_enum(), "enabled": true,
                "config": {}
            }]});
            let out = translate_policies(&platform);
            let pols = out["policies"].as_array().unwrap();
            assert_eq!(pols.len(), 1, "{} was dropped in translation", ty.as_str());
            assert_eq!(pols[0]["type"], ty.as_str());
        }
    }

    #[test]
    fn unrecognized_platform_type_keeps_its_raw_name() {
        // A type the platform invents that this build has never heard of must
        // reach the engine verbatim, so the rejection can quote it.
        let platform = json!({"policies": [
            {"policyId":"p","name":"n","type":"VIBE_CHECK","enabled":true,"config":{}}
        ]});
        let out = translate_policies(&platform);
        assert_eq!(out["policies"][0]["type"], "VIBE_CHECK");
        let bundle = translate_bundle(&platform).unwrap();
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());
        assert_eq!(engine.active_policy_count(), 0);
        assert!(engine.rejected_policies()[0].contains("VIBE_CHECK"));
    }

    #[test]
    fn mode_defaults_to_enforce_when_absent_or_null() {
        // The backend's `mode` field is deprecated and typically serialized as
        // `null` (or omitted). Enforcement keys off `enabled`, so a mode-less
        // enabled policy must ENFORCE — not silently degrade to shadow.
        let platform = json!({"policies": [
            {"policyId":"a","name":"no-mode","type":"COST_CAP","enabled":true,
             "config":{"window":"1d_rolling","maxUsd":5.0,"action":"BLOCK"}},
            {"policyId":"b","name":"null-mode","type":"COST_CAP","mode":null,"enabled":true,
             "config":{"window":"1d_rolling","maxUsd":5.0,"action":"BLOCK"}},
        ]});
        let out = translate_policies(&platform);
        let pols = out["policies"].as_array().unwrap();
        assert_eq!(pols.len(), 2);
        assert_eq!(pols[0]["mode"], "enforce", "absent mode → enforce");
        assert_eq!(pols[1]["mode"], "enforce", "null mode → enforce");
    }

    #[test]
    fn explicit_off_mode_is_skipped() {
        let platform = json!({"policies": [
            {"policyId":"a","name":"off","type":"COST_CAP","mode":"OFF","enabled":true,
             "config":{"window":"1d_rolling","maxUsd":5.0,"action":"BLOCK"}},
            {"policyId":"b","name":"on","type":"COST_CAP","mode":"ENFORCE","enabled":true,
             "config":{"window":"1d_rolling","maxUsd":5.0,"action":"BLOCK"}},
        ]});
        let out = translate_policies(&platform);
        let pols = out["policies"].as_array().unwrap();
        assert_eq!(pols.len(), 1, "OFF mode is dropped");
        assert_eq!(pols[0]["name"], "on");
    }

    #[test]
    fn enabled_mode_less_cost_cap_actually_blocks() {
        // End-to-end guard against the shadow-default regression: a platform
        // cost_cap with no `mode` and spend over the cap MUST produce a block.
        let platform = json!({"policies":[{"policyId":"x","name":"cap","type":"COST_CAP","enabled":true,
            "config":{"window":"1d_rolling","maxUsd":10.0,"action":"BLOCK"}}]});
        let bundle = translate_bundle(&platform).unwrap();
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());
        let live = state_to_live_state(&json!({"cost":{"1d_rolling": 25.0}, "rate":{}}));
        let result = engine.evaluate(Phase::Input, "gpt-4o", "hello", None, None, Some(&live));
        assert!(
            result.block.is_some(),
            "a mode-less enabled cost_cap over the cap must block"
        );
    }

    #[test]
    fn source_is_carried_through_translation() {
        let platform = json!({"policies": [
            {"policyId":"p1","name":"org cap","type":"COST_CAP","enabled":true,"source":"org",
             "config":{"window":"30d_rolling","maxUsd":100.0,"action":"BLOCK"}},
            {"policyId":"p2","name":"proj cap","type":"COST_CAP","enabled":true,"source":"project",
             "config":{"window":"1d_rolling","maxUsd":5.0,"action":"BLOCK"}}
        ]});
        let out = translate_policies(&platform);
        let pols = out["policies"].as_array().unwrap();
        assert_eq!(pols[0]["source"], "org");
        assert_eq!(pols[1]["source"], "project");
        // And it survives bundle parsing.
        let bundle = translate_bundle(&platform).unwrap();
        assert_eq!(bundle.policies[0].source.as_deref(), Some("org"));
    }

    #[test]
    fn state_parses_optional_org_scope() {
        let state = json!({
            "cost": {"1d_rolling": 1.0},
            "rate": {"requests_1m": 2},
            "org": {
                "cost": {"1d_rolling": 500.0},
                "rate": {"requests_1m": 999}
            }
        });
        let ls = state_to_live_state(&state);
        assert_eq!(ls.cost_usd_by_window.get("1d_rolling"), Some(&1.0));
        assert_eq!(ls.org_cost_usd_by_window.get("1d_rolling"), Some(&500.0));
        assert_eq!(ls.org_requests_by_window.get("1m"), Some(&999));
        assert!(ls.has_org_counters());
        // Without the org section the org maps stay empty.
        let plain = state_to_live_state(&json!({"cost":{"1d_rolling":1.0},"rate":{}}));
        assert!(!plain.has_org_counters());
    }

    #[test]
    fn state_maps_cost_windows_and_rate_periods() {
        let state = json!({
            "cost": {"1d_rolling": 12.5, "30d_rolling": 250.0, "perModel": {"gpt-4o": 5.0}},
            "rate": {"requests_1m": 42, "tokens_1m": 1000, "requests_1h": 500},
            "asOf": "2026-06-26T00:00:00Z", "ttlSeconds": 30
        });
        let ls = state_to_live_state(&state);
        assert_eq!(ls.cost_usd_by_window.get("1d_rolling"), Some(&12.5));
        assert_eq!(ls.cost_usd_by_window.get("30d_rolling"), Some(&250.0));
        assert!(!ls.cost_usd_by_window.contains_key("perModel"));
        assert_eq!(ls.requests_by_window.get("1m"), Some(&42));
        assert_eq!(ls.requests_by_window.get("1h"), Some(&500));
        assert_eq!(ls.tokens_by_window.get("1m"), Some(&1000));
    }

    #[test]
    fn cost_cap_blocks_when_state_exceeds_cap() {
        // End-to-end of the mapping: a translated ENFORCE cost_cap + a live state
        // over the cap must produce a blocking decision.
        let platform = json!({"policies":[{"name":"cap","type":"COST_CAP","mode":"ENFORCE","enabled":true,
            "config":{"window":"1d_rolling","maxUsd":10.0,"action":"BLOCK"}}]});
        let bundle = translate_bundle(&platform).unwrap();
        let engine = PolicyEngine::from_bundle(&bundle, EngineOptions::default());
        let live = state_to_live_state(&json!({"cost":{"1d_rolling": 25.0}, "rate":{}}));
        let result = engine.evaluate(Phase::Input, "gpt-4o", "hello", None, None, Some(&live));
        assert!(result.block.is_some(), "spend over cap must block");
    }
}
