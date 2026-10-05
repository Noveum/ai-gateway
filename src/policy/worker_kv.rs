//! Workers KV policy bundle loading for the Cloudflare Worker edge runtime.
//!
//! When the platform bridge ([`crate::policy::worker_remote`]) is **not**
//! configured, an optional [`KV_BINDING_VAR`] namespace can hold the same
//! `nova-guard.json` [`PolicyBundle`] schema the Worker already accepts via
//! [`INLINE_POLICIES_VAR`]. A write to that key changes enforcement for every
//! isolate without a Worker redeploy. Each isolate re-reads after about 60
//! seconds; Workers KV's own edge cache means a write can take longer than
//! that to arrive.
//!
//! Precedence (no platform bridge):
//! 1. **KV** — non-empty value at [`KV_POLICY_KEY`], when the binding is present.
//! 2. **Inline** — [`INLINE_POLICIES_VAR`] secret/var.
//! 3. **Transparent** — empty bundle, identical to the checked-in default.
//!
//! A missing or whitespace-only KV value falls through to inline, then empty.
//! Malformed JSON, an unsupported schema major, or a policy this build cannot
//! compile is a configuration error (`503`) for whichever source won — the same
//! refusal as native [`crate::policy::engine::PolicyEngine::from_env`]. A KV
//! transport failure is a `503` only when this isolate has never loaded a
//! bundle; after that, a failed refresh keeps the last good value.
//!
//! `NOVEUM_GUARD_ENABLED=false` does not read KV or inline JSON (see
//! `worker_rt`). With the platform bridge configured, the control plane remains
//! the source of truth; any KV binding or inline bundle is ignored (with a
//! warning).

#[cfg(target_arch = "wasm32")]
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use worker::console_error;

use crate::policy::config::PolicyBundle;
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
/// cache cadence in [`crate::policy::worker_remote`] (~60 s). Combined with
/// Workers KV's edge cache, a write can take up to about two minutes to reach
/// a given isolate.
#[cfg(any(target_arch = "wasm32", test))]
const KV_CACHE_TTL_MS: f64 = 60_000.0;

/// After a failed refresh that still has a last good value, skip another KV
/// read for this long. Matches the platform path's error backoff.
#[cfg(any(target_arch = "wasm32", test))]
const KV_ERROR_BACKOFF_MS: f64 = 5_000.0;

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

/// True when a non-empty KV value replaces a non-empty inline bundle.
///
/// `{}` and any other present value count: the operator's inline secret is no
/// longer what this isolate enforces. Whitespace-only KV still falls through.
pub fn kv_overrides_inline(kv: &KvPolicyLookup, inline: Option<&str>) -> bool {
    let kv_present = match kv {
        KvPolicyLookup::Present(raw) => !raw.trim().is_empty(),
        KvPolicyLookup::NotConfigured | KvPolicyLookup::Absent => false,
    };
    kv_present && inline.is_some_and(|s| !s.trim().is_empty())
}

/// Resolve the effective local policy bundle when the platform bridge is off.
///
/// Validates JSON, schema major, and compilability in precedence order: a
/// present KV value is checked before inline is considered. Returns
/// [`PolicyBundle::default()`] for a transparent proxy when neither source
/// yields a non-empty bundle. Whitespace-only [`KvPolicyLookup::Present`] is
/// treated as [`KvPolicyLookup::Absent`].
pub fn resolve_policy_bundle(
    kv: KvPolicyLookup,
    inline: Option<String>,
) -> Result<PolicyBundle, String> {
    let inline = inline.filter(|s| !s.trim().is_empty());
    let kv = match kv {
        KvPolicyLookup::Present(raw) if raw.trim().is_empty() => KvPolicyLookup::Absent,
        other => other,
    };

    if let KvPolicyLookup::Present(raw) = kv {
        return accept_local_bundle(&raw, &format!("Workers KV key `{KV_POLICY_KEY}`"));
    }

    match inline {
        None => Ok(PolicyBundle::default()),
        Some(s) => accept_local_bundle(&s, INLINE_POLICIES_VAR),
    }
}

fn accept_local_bundle(raw: &str, source: &str) -> Result<PolicyBundle, String> {
    let bundle = PolicyBundle::from_json_str(raw)
        .map_err(|e| format!("{source} is set but is not a valid nova-guard bundle: {e}"))?;
    bundle
        .schema_version_supported()
        .map_err(|e| format!("{source} is set but this build cannot enforce it: {e}"))?;
    refuse_uncompilable(&bundle).map_err(|e| format!("{source}: {e}"))?;
    Ok(bundle)
}

/// Same refusal as native `PolicyEngine::from_env`: an active policy this build
/// cannot compile must not become a silent pass-through.
fn refuse_uncompilable(bundle: &PolicyBundle) -> Result<(), String> {
    let rejected = PolicyEngine::from_bundle(bundle, EngineOptions::default()).rejected_policies();
    if rejected.is_empty() {
        return Ok(());
    }
    // `tracing::error!` inside compile has no subscriber on wasm, so each
    // rejection has to be a `console_error` or it never reaches `wrangler tail`.
    #[cfg(target_arch = "wasm32")]
    {
        for line in &rejected {
            console_error!("Nova Guard: local policy rejected: {line}");
        }
    }
    Err(format!(
        "Nova Guard policy bundle contains {} policy/policies this build cannot enforce:\n  - {}",
        rejected.len(),
        rejected.join("\n  - ")
    ))
}

#[derive(Clone)]
#[cfg(any(target_arch = "wasm32", test))]
struct CachedKvPolicy {
    lookup: KvPolicyLookup,
    fetched_ms: f64,
    last_error_ms: Option<f64>,
}

/// Isolate-local KV cache with a monotonic compare-and-swap generation.
///
/// Workers run a single-threaded event loop, but requests interleave at
/// `.await`. A later refresh can finish before an earlier one. Only a
/// generation newer than the last applied success may replace the cached
/// lookup. A failed refresh does not advance that generation, so an older
/// in-flight success can still install, and it does not drop a lookup that
/// already loaded.
#[derive(Default)]
#[cfg(any(target_arch = "wasm32", test))]
struct KvLookupCache {
    current: Option<CachedKvPolicy>,
    next_generation: u64,
    applied_generation: u64,
}

#[cfg(any(target_arch = "wasm32", test))]
impl KvLookupCache {
    fn begin_refresh(&mut self) -> u64 {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("a Worker isolate cannot start 2^64 KV policy refreshes");
        self.next_generation
    }

    /// Serve the cached lookup when it is inside the TTL or the error backoff.
    fn fresh_or_backing_off(&self, now_ms: f64) -> Option<KvPolicyLookup> {
        let cached = self.current.as_ref()?;
        let fresh = now_ms - cached.fetched_ms < KV_CACHE_TTL_MS;
        let backing_off = cached
            .last_error_ms
            .is_some_and(|t| now_ms - t < KV_ERROR_BACKOFF_MS);
        (fresh || backing_off).then(|| cached.lookup.clone())
    }

    /// Install `lookup` only if no newer success already won.
    fn apply_success(&mut self, generation: u64, lookup: KvPolicyLookup, now_ms: f64) -> bool {
        if generation <= self.applied_generation {
            return false;
        }
        self.applied_generation = generation;
        self.current = Some(CachedKvPolicy {
            lookup,
            fetched_ms: now_ms,
            last_error_ms: None,
        });
        true
    }

    /// Record a refresh failure without advancing the applied generation.
    ///
    /// Returns the last successful lookup when one exists. An error older than
    /// an already-applied success does not mark that success unhealthy.
    fn note_refresh_error(&mut self, generation: u64, now_ms: f64) -> Option<KvPolicyLookup> {
        if generation <= self.applied_generation {
            return self.current.as_ref().map(|entry| entry.lookup.clone());
        }
        let entry = self.current.as_mut()?;
        entry.last_error_ms = Some(now_ms);
        Some(entry.lookup.clone())
    }
}

#[cfg(target_arch = "wasm32")]
struct KvLookupOutcome {
    lookup: KvPolicyLookup,
    /// True when this call installed a new KV value into the isolate cache.
    applied: bool,
}

#[cfg(target_arch = "wasm32")]
mod wasm_io {
    use super::*;
    use std::cell::RefCell;

    use worker::{console_warn, js_sys, Env, Result};

    thread_local! {
        static KV_POLICY_CACHE: RefCell<KvLookupCache> = RefCell::new(KvLookupCache::default());
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

    async fn kv_policy_lookup(env: &Env) -> Result<KvLookupOutcome, String> {
        if !kv_binding_present(env) {
            return Ok(KvLookupOutcome {
                lookup: KvPolicyLookup::NotConfigured,
                applied: false,
            });
        }

        let now = now_ms();
        if let Some(lookup) = KV_POLICY_CACHE.with(|cache| cache.borrow().fresh_or_backing_off(now))
        {
            return Ok(KvLookupOutcome {
                lookup,
                applied: false,
            });
        }

        let generation = KV_POLICY_CACHE.with(|cache| cache.borrow_mut().begin_refresh());
        match read_kv_text(env).await {
            Ok(lookup) => {
                let applied = KV_POLICY_CACHE.with(|cache| {
                    cache
                        .borrow_mut()
                        .apply_success(generation, lookup.clone(), now_ms())
                });
                Ok(KvLookupOutcome { lookup, applied })
            }
            Err(e) => {
                let stale = KV_POLICY_CACHE
                    .with(|cache| cache.borrow_mut().note_refresh_error(generation, now_ms()));
                match stale {
                    Some(lookup) => {
                        console_warn!(
                            "Nova Guard: Workers KV policy refresh failed ({e}); keeping the last bundle that loaded"
                        );
                        Ok(KvLookupOutcome {
                            lookup,
                            applied: false,
                        })
                    }
                    None => Err(e),
                }
            }
        }
    }

    /// Build a Nova Guard engine from KV (when bound) and/or inline env, without
    /// the platform bridge.
    pub async fn local_policy_engine(
        env: &Env,
        opts: EngineOptions,
        inline: Option<String>,
    ) -> Result<Rc<PolicyEngine>, String> {
        let read = kv_policy_lookup(env).await?;
        if read.applied && kv_overrides_inline(&read.lookup, inline.as_deref()) {
            console_warn!(
                "Nova Guard: Workers KV key `{KV_POLICY_KEY}` overrides non-empty \
                 {INLINE_POLICIES_VAR}; a KV write changes enforcement for every isolate \
                 without a Worker redeploy"
            );
        }
        let bundle = resolve_policy_bundle(read.lookup, inline)?;
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
        let bundle = resolve_policy_bundle(
            KvPolicyLookup::Present("  \n".to_string()),
            Some(VALID.to_string()),
        )
        .unwrap();
        assert_eq!(bundle.policies.len(), 1);
        assert_eq!(bundle.policies[0].name, "block");
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
    fn unsupported_schema_major_is_a_configuration_error() {
        let err = resolve_policy_bundle(
            KvPolicyLookup::Absent,
            Some(r#"{"schemaVersion":"2.0.0","policies":[]}"#.to_string()),
        )
        .unwrap_err();
        assert!(err.contains(INLINE_POLICIES_VAR), "{err}");
        assert!(err.contains("schemaVersion"), "{err}");
        assert!(err.contains("major 2"), "{err}");
    }

    #[test]
    fn unknown_policy_type_is_a_configuration_error() {
        let err = resolve_policy_bundle(
            KvPolicyLookup::Present(
                r#"{"policies":[{"name":"typo","type":"promt_injection","mode":"enforce","config":{}}]}"#
                    .to_string(),
            ),
            Some(VALID_INLINE.to_string()),
        )
        .unwrap_err();
        assert!(err.contains(KV_POLICY_KEY), "{err}");
        assert!(err.contains("promt_injection"), "{err}");
        assert!(err.contains("cannot enforce"), "{err}");
    }

    #[test]
    fn kv_override_requires_non_empty_kv_and_inline() {
        assert!(kv_overrides_inline(
            &KvPolicyLookup::Present(VALID.to_string()),
            Some(VALID_INLINE),
        ));
        assert!(kv_overrides_inline(
            &KvPolicyLookup::Present("{}".to_string()),
            Some(VALID_INLINE),
        ));
        assert!(!kv_overrides_inline(
            &KvPolicyLookup::Present(VALID.to_string()),
            None,
        ));
        assert!(!kv_overrides_inline(
            &KvPolicyLookup::Present(VALID.to_string()),
            Some("  \n"),
        ));
        assert!(!kv_overrides_inline(
            &KvPolicyLookup::Present(" \n".to_string()),
            Some(VALID_INLINE),
        ));
        assert!(!kv_overrides_inline(
            &KvPolicyLookup::Absent,
            Some(VALID_INLINE),
        ));
        assert!(!kv_overrides_inline(
            &KvPolicyLookup::NotConfigured,
            Some(VALID_INLINE)
        ));
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
    fn older_kv_refresh_cannot_replace_a_newer_one() {
        let mut cache = KvLookupCache::default();
        let older = cache.begin_refresh();
        let newer = cache.begin_refresh();
        assert!(cache.apply_success(newer, KvPolicyLookup::Present("new".to_string()), 1_000.0));
        assert!(!cache.apply_success(older, KvPolicyLookup::Present("old".to_string()), 1_100.0));
        assert_eq!(
            cache.fresh_or_backing_off(1_100.0),
            Some(KvPolicyLookup::Present("new".to_string()))
        );
    }

    #[test]
    fn kv_refresh_error_without_a_cached_bundle_is_a_miss() {
        let mut cache = KvLookupCache::default();
        let generation = cache.begin_refresh();
        assert!(cache.note_refresh_error(generation, 0.0).is_none());
        assert!(cache.fresh_or_backing_off(1.0).is_none());
    }

    #[test]
    fn kv_refresh_error_keeps_the_last_good_bundle() {
        let mut cache = KvLookupCache::default();
        let loaded = cache.begin_refresh();
        assert!(cache.apply_success(loaded, KvPolicyLookup::Absent, 0.0));
        let failed = cache.begin_refresh();
        assert_eq!(
            cache.note_refresh_error(failed, 70_000.0),
            Some(KvPolicyLookup::Absent)
        );
        assert_eq!(
            cache.fresh_or_backing_off(70_000.0),
            Some(KvPolicyLookup::Absent)
        );
        assert!(cache.fresh_or_backing_off(80_000.0).is_none());
    }

    #[test]
    fn failed_kv_refresh_does_not_block_an_older_in_flight_success() {
        let mut cache = KvLookupCache::default();
        let slow = cache.begin_refresh();
        let failed = cache.begin_refresh();
        assert!(cache.note_refresh_error(failed, 10.0).is_none());
        assert!(cache.apply_success(slow, KvPolicyLookup::Present("slow".to_string()), 20.0));
        assert_eq!(
            cache.fresh_or_backing_off(20.0),
            Some(KvPolicyLookup::Present("slow".to_string()))
        );
    }

    #[test]
    fn kv_error_older_than_an_applied_success_does_not_arm_backoff() {
        let mut cache = KvLookupCache::default();
        let older = cache.begin_refresh();
        let newer = cache.begin_refresh();
        assert!(cache.apply_success(newer, KvPolicyLookup::Present("new".to_string()), 0.0));
        assert_eq!(
            cache.note_refresh_error(older, 70_000.0),
            Some(KvPolicyLookup::Present("new".to_string()))
        );
        assert!(cache.fresh_or_backing_off(70_000.0).is_none());
    }
}
