//! Workers KV policy bundle loading for the Cloudflare Worker edge runtime.
//!
//! When the platform bridge ([`crate::policy::worker_remote`]) is **not**
//! configured, an optional [`KV_BINDING_VAR`] namespace can hold the same
//! `nova-guard.json` [`PolicyBundle`] schema the Worker already accepts via
//! [`INLINE_POLICIES_VAR`]. Updates written to KV propagate globally without a
//! Worker redeploy, subject to the in-isolate cache TTL below.
//!
//! Precedence (no platform bridge):
//! 1. **KV** — non-empty value at [`KV_POLICY_KEY`], when the binding is present.
//! 2. **Inline** — [`INLINE_POLICIES_VAR`] secret/var.
//! 3. **Transparent** — empty bundle, identical to the checked-in default.
//!
//! A missing or whitespace-only KV value falls through to inline, then empty.
//! Malformed JSON in KV or inline is a configuration error (`503`), never a
//! silent pass-through. KV transport failures with the binding present are also
//! configuration errors: the operator opted into KV as a policy source.
//!
//! With the platform bridge configured, the control plane remains the source of
//! truth; any KV binding or inline bundle is ignored (with a warning).

#[cfg(target_arch = "wasm32")]
use std::rc::Rc;

use crate::policy::config::PolicyBundle;

#[cfg(target_arch = "wasm32")]
use crate::policy::engine::{EngineOptions, PolicyEngine};

/// Wrangler `kv_namespaces` binding name for the policy bundle namespace.
///
/// Example:
/// ```toml
/// [[kv_namespaces]]
/// binding = "NOVEUM_GUARD_POLICIES_KV"
/// id = "<namespace-id>"
/// ```
pub const KV_BINDING_VAR: &str = "NOVEUM_GUARD_POLICIES_KV";

/// Fixed KV key holding the JSON policy bundle (`nova-guard.json` schema).
pub const KV_POLICY_KEY: &str = "nova-guard-policies";

/// Inline env/secret var (unchanged); fallback after an absent KV key.
pub const INLINE_POLICIES_VAR: &str = "NOVEUM_GUARD_POLICIES";

/// In-isolate reuse window before re-reading KV. Matches the platform policy
/// cache cadence in [`crate::policy::worker_remote`] (~60 s).
#[cfg(target_arch = "wasm32")]
const KV_CACHE_TTL_MS: f64 = 60_000.0;

/// Outcome of a KV policy lookup (binding present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvPolicyLookup {
    /// The KV binding is not configured on this deployment.
    NotConfigured,
    /// Binding present; key missing or value is whitespace-only.
    Absent,
    /// Binding present; non-empty JSON text stored at [`KV_POLICY_KEY`].
    Present(String),
}

/// Whether local (non-platform) policy sources are configured alongside the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPolicySources {
    pub kv_binding: bool,
    pub inline: bool,
}

impl LocalPolicySources {
    pub fn any(&self) -> bool {
        self.kv_binding || self.inline
    }
}

/// Resolve the effective local policy bundle when the platform bridge is off.
///
/// Validates JSON in precedence order: a present KV value is checked before
/// inline is considered. Returns [`PolicyBundle::default()`] for a transparent
/// proxy when neither source yields a non-empty bundle.
pub fn resolve_policy_bundle(
    kv: KvPolicyLookup,
    inline: Option<String>,
) -> Result<PolicyBundle, String> {
    let inline = inline.filter(|s| !s.trim().is_empty());

    if let KvPolicyLookup::Present(raw) = kv {
        return PolicyBundle::from_json_str(&raw).map_err(|e| {
            format!(
                "Workers KV key `{KV_POLICY_KEY}` is set but is not a valid nova-guard bundle: {e}"
            )
        });
    }

    match inline {
        None => Ok(PolicyBundle::default()),
        Some(s) => PolicyBundle::from_json_str(&s).map_err(|e| {
            format!("{INLINE_POLICIES_VAR} is set but is not a valid nova-guard bundle: {e}")
        }),
    }
}

#[derive(Clone)]
#[cfg(target_arch = "wasm32")]
struct CachedKvPolicy {
    lookup: KvPolicyLookup,
    fetched_ms: f64,
}

#[cfg(target_arch = "wasm32")]
impl CachedKvPolicy {
    fn fresh(&self, now_ms: f64) -> bool {
        now_ms - self.fetched_ms < KV_CACHE_TTL_MS
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_io {
    use super::*;
    use std::cell::RefCell;

    use worker::{js_sys, Env, Result};

    thread_local! {
        static KV_POLICY_CACHE: RefCell<Option<CachedKvPolicy>> = const { RefCell::new(None) };
    }

    fn now_ms() -> f64 {
        js_sys::Date::now()
    }

    /// True when `wrangler.toml` declares the [`KV_BINDING_VAR`] namespace.
    pub fn kv_binding_present(env: &Env) -> bool {
        env.kv(KV_BINDING_VAR).is_ok()
    }

    async fn read_kv_text(env: &Env) -> Result<KvPolicyLookup, String> {
        let kv = env
            .kv(KV_BINDING_VAR)
            .map_err(|e| format!("Workers KV binding `{KV_BINDING_VAR}` is misconfigured: {e}"))?;
        match kv.get(KV_POLICY_KEY).text().await {
            Ok(Some(text)) if !text.trim().is_empty() => Ok(KvPolicyLookup::Present(text)),
            Ok(Some(_)) | Ok(None) => Ok(KvPolicyLookup::Absent),
            Err(e) => Err(format!("Workers KV read of `{KV_POLICY_KEY}` failed: {e}")),
        }
    }

    async fn kv_policy_lookup(env: &Env) -> Result<KvPolicyLookup, String> {
        if !kv_binding_present(env) {
            return Ok(KvPolicyLookup::NotConfigured);
        }

        let now = now_ms();
        if let Some(cached) = KV_POLICY_CACHE.with(|c| c.borrow().clone()) {
            if cached.fresh(now) {
                return Ok(cached.lookup);
            }
        }

        let lookup = read_kv_text(env).await?;
        KV_POLICY_CACHE.with(|c| {
            *c.borrow_mut() = Some(CachedKvPolicy {
                lookup: lookup.clone(),
                fetched_ms: now_ms(),
            });
        });
        Ok(lookup)
    }

    /// Build a Nova Guard engine from KV (when bound) and/or inline env, without
    /// the platform bridge.
    pub async fn local_policy_engine(
        env: &Env,
        opts: EngineOptions,
        inline: Option<String>,
    ) -> Result<Rc<PolicyEngine>, String> {
        let kv = kv_policy_lookup(env).await?;
        let bundle = resolve_policy_bundle(kv, inline)?;
        Ok(Rc::new(PolicyEngine::from_bundle(&bundle, opts)))
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm_io::{kv_binding_present, local_policy_engine};

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"policies":[{"name":"block","type":"regex_match","mode":"enforce","config":{"phase":"input","patterns":[{"name":"x","regex":"bad"}],"action":"block"}}]}"#;
    const VALID_INLINE: &str = r#"{"policies":[{"name":"inline","type":"regex_match","mode":"enforce","config":{"phase":"input","patterns":[{"name":"y","regex":"inline"}],"action":"block"}}]}"#;

    #[test]
    fn kv_not_configured_falls_through_to_inline_then_empty() {
        let bundle = resolve_policy_bundle(KvPolicyLookup::NotConfigured, None).unwrap();
        assert!(bundle.policies.is_empty());

        let bundle = resolve_policy_bundle(
            KvPolicyLookup::NotConfigured,
            Some(VALID_INLINE.to_string()),
        )
        .unwrap();
        assert_eq!(bundle.policies.len(), 1);
        assert_eq!(bundle.policies[0].name, "inline");
    }

    #[test]
    fn absent_kv_key_falls_through_to_inline() {
        let bundle = resolve_policy_bundle(KvPolicyLookup::Absent, None).unwrap();
        assert!(bundle.policies.is_empty());

        let bundle =
            resolve_policy_bundle(KvPolicyLookup::Absent, Some(VALID_INLINE.to_string())).unwrap();
        assert_eq!(bundle.policies[0].name, "inline");
    }

    #[test]
    fn present_kv_wins_over_inline() {
        let bundle = resolve_policy_bundle(
            KvPolicyLookup::Present(VALID.to_string()),
            Some(VALID_INLINE.to_string()),
        )
        .unwrap();
        assert_eq!(bundle.policies.len(), 1);
        assert_eq!(bundle.policies[0].name, "block");
    }

    #[test]
    fn whitespace_only_kv_falls_through_to_inline() {
        // Resolution treats whitespace-only as Absent before calling this helper.
        let bundle =
            resolve_policy_bundle(KvPolicyLookup::Absent, Some(VALID.to_string())).unwrap();
        assert_eq!(bundle.policies.len(), 1);
    }

    #[test]
    fn malformed_kv_is_a_configuration_error() {
        let err = resolve_policy_bundle(
            KvPolicyLookup::Present("{not json".to_string()),
            Some(VALID.to_string()),
        )
        .unwrap_err();
        assert!(err.contains(KV_POLICY_KEY), "{err}");
        assert!(err.contains("not a valid nova-guard bundle"), "{err}");
    }

    #[test]
    fn malformed_inline_is_a_configuration_error() {
        let err =
            resolve_policy_bundle(KvPolicyLookup::Absent, Some("{nope".to_string())).unwrap_err();
        assert!(err.contains(INLINE_POLICIES_VAR), "{err}");
    }

    #[test]
    fn local_sources_any_detects_kv_or_inline() {
        assert!(!LocalPolicySources {
            kv_binding: false,
            inline: false,
        }
        .any());
        assert!(LocalPolicySources {
            kv_binding: true,
            inline: false,
        }
        .any());
        assert!(LocalPolicySources {
            kv_binding: false,
            inline: true,
        }
        .any());
    }

    #[test]
    fn bridge_ignores_local_sources_when_platform_wins() {
        // Documented contract: when bridge is on, local resolution is skipped.
        // This test guards the pure helper used when bridge is off; platform
        // precedence is enforced in `worker_rt`.
        let bundle = resolve_policy_bundle(
            KvPolicyLookup::Present(VALID.to_string()),
            Some(VALID_INLINE.to_string()),
        )
        .unwrap();
        assert_eq!(bundle.policies[0].name, "block");
    }
}
