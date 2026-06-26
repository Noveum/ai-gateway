//! Native HTTP bridge to the Noveum platform NovaGuard API.
//!
//! When `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` are set, the gateway fetches
//! its Nova Guard policies from the platform on startup (instead of, or in
//! addition to, the local bundle) and queries the live cost/rate counters
//! (`.../policies/state`) per request so `cost_cap`/`rate_limit` enforce against
//! real spend instead of failing open. The pure JSON↔gateway mapping lives in
//! [`crate::policy::platform`]; this module is the native HTTP + cache layer.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::warn;

use crate::policy::config::PolicyBundle;
use crate::policy::platform;
use crate::policy::rules::LiveState;

/// Default platform base URL when `NOVEUM_API_URL` is unset.
const DEFAULT_API_URL: &str = "https://api.noveum.ai";
/// How long a fetched live-state snapshot is reused before refetching. The
/// platform's `/state` is itself cached ~30s, so a short client TTL is plenty.
const STATE_TTL: Duration = Duration::from_secs(10);

/// Connection details for the platform NovaGuard API.
#[derive(Clone, Debug)]
pub struct RemoteConfig {
    pub base_url: String,
    pub api_key: String,
    pub project_id: String,
}

impl RemoteConfig {
    /// Build from env, or `None` if platform-managed Nova Guard isn't configured
    /// (`NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` both required).
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("NOVEUM_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let project_id = std::env::var("NOVEUM_GUARD_PROJECT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let base_url = std::env::var("NOVEUM_API_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        Some(Self {
            base_url,
            api_key,
            project_id,
        })
    }

    fn policies_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies",
            self.base_url, self.project_id
        )
    }

    fn state_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/state",
            self.base_url, self.project_id
        )
    }
}

/// Fetch + translate the platform's policies into a gateway [`PolicyBundle`].
pub async fn fetch_bundle(cfg: &RemoteConfig) -> Result<PolicyBundle, String> {
    let resp = crate::proxy::CLIENT
        .get(cfg.policies_url())
        .bearer_auth(&cfg.api_key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("policies fetch returned {status}: {json}"));
    }
    platform::translate_bundle(&json)
}

async fn fetch_state(cfg: &RemoteConfig) -> Result<LiveState, String> {
    let resp = crate::proxy::CLIENT
        .get(cfg.state_url())
        .bearer_auth(&cfg.api_key)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("state fetch returned {status}: {json}"));
    }
    Ok(platform::state_to_live_state(&json))
}

/// A cached provider of the platform's live cost/rate counters, used by the guard
/// middleware to supply `LiveState` to `cost_cap`/`rate_limit` enforcement.
pub struct RemoteLiveState {
    cfg: RemoteConfig,
    cache: Mutex<Option<(Instant, LiveState)>>,
}

impl RemoteLiveState {
    pub fn new(cfg: RemoteConfig) -> Self {
        Self {
            cfg,
            cache: Mutex::new(None),
        }
    }

    /// Return the current live state, refetching if the cache is older than
    /// [`STATE_TTL`]. On fetch error, falls back to the last cached snapshot (even
    /// if stale) so a transient platform blip doesn't flip every policy open.
    pub async fn get(&self) -> Option<LiveState> {
        if let Some((at, ls)) = self.cache.lock().await.as_ref() {
            if at.elapsed() < STATE_TTL {
                return Some(ls.clone());
            }
        }
        match fetch_state(&self.cfg).await {
            Ok(ls) => {
                *self.cache.lock().await = Some((Instant::now(), ls.clone()));
                Some(ls)
            }
            Err(e) => {
                warn!(error = %e, "Nova Guard: live-state fetch failed; using last cached snapshot");
                self.cache.lock().await.as_ref().map(|(_, ls)| ls.clone())
            }
        }
    }
}
