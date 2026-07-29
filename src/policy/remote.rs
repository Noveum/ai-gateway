//! Native HTTP bridge to the Noveum platform NovaGuard API.
//!
//! When `NOVEUM_API_KEY` + `NOVEUM_GUARD_PROJECT_ID` are set, the gateway fetches
//! its Nova Guard policies from the platform on startup (instead of, or in
//! addition to, the local bundle) and queries the live cost/rate counters
//! (`.../policies/state`) per request so `cost_cap`/`rate_limit` enforce against
//! real spend instead of failing open. The pure JSON↔gateway mapping lives in
//! [`crate::policy::platform`]; this module is the native HTTP + cache layer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::policy::config::PolicyBundle;
use crate::policy::engine::PolicyEngine;
use crate::policy::platform;
use crate::policy::rules::LiveState;

/// Dedicated HTTP client for the Noveum platform API. Unlike `proxy::CLIENT`
/// (which forces HTTP/2 prior knowledge for HTTPS provider calls), this
/// negotiates the HTTP version normally so it works against a plain HTTP/1.1
/// control plane (e.g. a local `http://localhost:3000`) as well as HTTPS.
pub(crate) static PLATFORM_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build Noveum platform HTTP client")
});

/// Default platform base URL when `NOVEUM_API_URL` is unset.
const DEFAULT_API_URL: &str = "https://api.noveum.ai";
/// How long a fetched live-state snapshot is reused before refetching. The
/// platform's `/state` is itself cached ~30s, so a short client TTL is plenty.
const STATE_TTL: Duration = Duration::from_secs(10);
/// How often the background poller re-fetches policy definitions from
/// `/policies/effective`. The platform sets `Cache-Control: max-age=30`; ~60s
/// (with conditional `If-None-Match`) keeps policies fresh cheaply.
const POLICY_POLL_INTERVAL: Duration = Duration::from_secs(60);

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
        // `/effective` returns the org+project *merged*, enabled-only,
        // priority-ordered set (the plain `/policies` list is project-local,
        // unmerged, and includes disabled rows). The SDK enforces the effective
        // set, so the gateway must too.
        format!(
            "{}/api/v1/projects/{}/policies/effective",
            self.base_url, self.project_id
        )
    }

    fn state_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/state",
            self.base_url, self.project_id
        )
    }

    /// `POST` target for reporting per-call usage (ALLOWED/BLOCKED events).
    pub(crate) fn usage_url(&self) -> String {
        format!(
            "{}/api/v1/projects/{}/policies/usage",
            self.base_url, self.project_id
        )
    }
}

/// Result of a conditional policy fetch.
pub enum PolicyFetch {
    /// The platform returned `304 Not Modified` — keep the current policies.
    NotModified,
    /// A fresh policy set (with its `ETag`, if any, for the next conditional GET).
    Modified {
        bundle: PolicyBundle,
        etag: Option<String>,
    },
}

/// Fetch + translate the platform's effective policies, sending `If-None-Match`
/// when a prior `ETag` is known so an unchanged policy set costs a cheap `304`.
pub async fn fetch_bundle_conditional(
    cfg: &RemoteConfig,
    prev_etag: Option<&str>,
) -> Result<PolicyFetch, String> {
    let mut req = PLATFORM_CLIENT
        .get(cfg.policies_url())
        .bearer_auth(&cfg.api_key);
    if let Some(tag) = prev_etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(PolicyFetch::NotModified);
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("policies fetch returned {status}: {json}"));
    }
    let bundle = platform::translate_bundle(&json)?;
    Ok(PolicyFetch::Modified { bundle, etag })
}

/// Fetch + translate the platform's effective policies, returning the `ETag` too
/// so the caller can seed the background poller.
pub async fn fetch_bundle_with_etag(
    cfg: &RemoteConfig,
) -> Result<(PolicyBundle, Option<String>), String> {
    match fetch_bundle_conditional(cfg, None).await? {
        PolicyFetch::Modified { bundle, etag } => Ok((bundle, etag)),
        PolicyFetch::NotModified => Err("unexpected 304 without a prior ETag".to_string()),
    }
}

/// Spawn a background task that re-fetches `/policies/effective` every
/// [`POLICY_POLL_INTERVAL`] and hot-swaps the engine's policy set when it
/// changes. Uses `If-None-Match`/304 so an unchanged set is nearly free.
///
/// A fetch *error* is logged and the current policies are kept (a transient
/// platform blip never wipes enforcement). A *successful* empty set, by contrast,
/// is applied — the platform is the source of truth, and a project legitimately
/// having zero policies must clear the local set (mirrors the startup fetch).
pub fn spawn_policy_poller(
    cfg: RemoteConfig,
    engine: Arc<PolicyEngine>,
    initial_etag: Option<String>,
) {
    tokio::spawn(async move {
        let mut etag = initial_etag;
        let mut ticker = tokio::time::interval(POLICY_POLL_INTERVAL);
        ticker.tick().await; // consume the immediate first tick (already loaded at startup)
        loop {
            ticker.tick().await;
            match fetch_bundle_conditional(&cfg, etag.as_deref()).await {
                Ok(PolicyFetch::NotModified) => {
                    debug!("Nova Guard: policies unchanged (304)");
                }
                Ok(PolicyFetch::Modified { bundle, etag: new }) => {
                    engine.swap_bundle(&bundle);
                    etag = new;
                    info!(
                        active = engine.active_policy_count(),
                        "Nova Guard: policies refreshed from the platform"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Nova Guard: policy refresh failed; keeping current policies");
                }
            }
        }
    });
}

/// A cached snapshot of the platform's live state, plus the metadata needed for
/// conditional revalidation.
struct StateSnapshot {
    at: Instant,
    state: LiveState,
    etag: Option<String>,
}

struct StateCache {
    snap: Option<StateSnapshot>,
    /// When the current in-flight refresh started (`None` = none in flight).
    /// Single-flights the refetch so a burst of concurrent requests hitting an
    /// expired TTL doesn't stampede `/state` with N identical GETs.
    refreshing_since: Option<Instant>,
}

/// How long a `refreshing` flag may persist before another caller is allowed to
/// retry — guards against a refresh future being cancelled (e.g. the client
/// disconnected mid-fetch) and leaving the flag stuck forever. Comfortably above
/// the platform client's request timeout.
const REFRESH_STUCK_AFTER: Duration = Duration::from_secs(15);

/// A cached provider of the platform's live cost/rate counters, used by the guard
/// middleware to supply `LiveState` to `cost_cap`/`rate_limit` enforcement.
pub struct RemoteLiveState {
    cfg: RemoteConfig,
    cache: Mutex<StateCache>,
}

impl RemoteLiveState {
    pub fn new(cfg: RemoteConfig) -> Self {
        Self {
            cfg,
            cache: Mutex::new(StateCache {
                snap: None,
                refreshing_since: None,
            }),
        }
    }

    /// Return the current live state, revalidating if the cache is older than
    /// [`STATE_TTL`]. Sends `If-None-Match`; a `304` just refreshes the snapshot's
    /// timestamp. On fetch error, falls back to the last cached snapshot (even if
    /// stale) so a transient platform blip doesn't flip every policy open.
    ///
    /// Refreshes are single-flighted: while one caller is refetching an expired
    /// snapshot, concurrent callers return the last snapshot (or `None` if there
    /// is none yet) rather than each issuing their own `/state` request.
    pub async fn get(&self) -> Option<LiveState> {
        let prev_etag = {
            let mut guard = self.cache.lock().await;
            if let Some(snap) = &guard.snap {
                if snap.at.elapsed() < STATE_TTL {
                    return Some(snap.state.clone());
                }
            }
            // Stale or empty. If another caller is already refreshing (and hasn't
            // gotten stuck), serve the stale snapshot instead of piling on.
            let in_flight = guard
                .refreshing_since
                .is_some_and(|t| t.elapsed() < REFRESH_STUCK_AFTER);
            if in_flight {
                return guard.snap.as_ref().map(|s| s.state.clone());
            }
            guard.refreshing_since = Some(Instant::now());
            guard.snap.as_ref().and_then(|s| s.etag.clone())
        };

        let result = fetch_state_conditional(&self.cfg, prev_etag.as_deref()).await;
        let mut guard = self.cache.lock().await;
        guard.refreshing_since = None; // refresh done (success or failure)
        match result {
            Ok(StateFetch::NotModified) => {
                if let Some(snap) = guard.snap.as_mut() {
                    snap.at = Instant::now(); // revalidated; reset the freshness clock
                    return Some(snap.state.clone());
                }
                // A 304 with no cached snapshot shouldn't happen (we only send
                // If-None-Match when we have one) but a caching proxy in front of
                // the platform could produce it — treat as no live state.
                warn!("Nova Guard: live-state returned 304 with no cached snapshot; treating as unavailable");
                None
            }
            Ok(StateFetch::Modified { state, etag, stale }) => {
                if stale {
                    // Counters came from the durable fallback (cache was down);
                    // they're conservative but real, so we still enforce against
                    // them — just make the condition observable.
                    warn!("Nova Guard: live-state served stale (durable fallback)");
                }
                guard.snap = Some(StateSnapshot {
                    at: Instant::now(),
                    state: state.clone(),
                    etag,
                });
                Some(state)
            }
            Err(e) => {
                warn!(error = %e, "Nova Guard: live-state fetch failed; using last cached snapshot");
                guard.snap.as_ref().map(|s| s.state.clone())
            }
        }
    }
}

/// Result of a conditional `/state` fetch.
enum StateFetch {
    NotModified,
    Modified {
        state: LiveState,
        etag: Option<String>,
        stale: bool,
    },
}

async fn fetch_state_conditional(
    cfg: &RemoteConfig,
    prev_etag: Option<&str>,
) -> Result<StateFetch, String> {
    let mut req = PLATFORM_CLIENT
        .get(cfg.state_url())
        .bearer_auth(&cfg.api_key);
    if let Some(tag) = prev_etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, tag);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(StateFetch::NotModified);
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    if !status.is_success() {
        return Err(format!("state fetch returned {status}: {json}"));
    }
    let stale = json.get("stale").and_then(|s| s.as_bool()).unwrap_or(false);
    Ok(StateFetch::Modified {
        state: platform::state_to_live_state(&json),
        etag,
        stale,
    })
}
