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
        // A stable, explicit User-Agent. The production api.noveum.ai edge
        // (CDN/WAF) rejects requests with no UA with a 403 before they reach the
        // application, which silently killed the whole platform bridge (policies,
        // state, and usage all go through this client).
        .user_agent(concat!("noveum-ai-gateway/", env!("CARGO_PKG_VERSION")))
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
    // Check the status BEFORE parsing: error bodies are often not JSON at all
    // (a CDN/WAF 403 serves HTML), and "invalid JSON" would mask the real
    // failure. Preserve the raw body in the error instead.
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "policies fetch returned {status}: {}",
            truncate_body(&body)
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    let bundle = platform::translate_bundle(&json)?;
    Ok(PolicyFetch::Modified { bundle, etag })
}

/// Trim an error body for logging (WAF/CDN error pages can be large HTML).
fn truncate_body(body: &str) -> String {
    const MAX: usize = 512;
    let trimmed = body.trim();
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut end = MAX;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… ({} bytes)", &trimmed[..end], trimmed.len())
    }
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
    /// When the last refresh attempt *failed* (`None` = last attempt succeeded).
    /// Within [`ERROR_BACKOFF`] of a failure, callers get `None` (state
    /// unavailable) immediately instead of re-hitting an unreachable platform on
    /// every request.
    last_error_at: Option<Instant>,
}

/// After a failed refresh, how long callers report "state unavailable" without
/// retrying the fetch. Keeps an outage from adding connect-timeout latency to
/// every guarded request while staying short enough to recover quickly.
const ERROR_BACKOFF: Duration = Duration::from_secs(3);

/// Hard bound on how long an in-request `/state` refresh may run. The refresh
/// happens on the request path with the cache lock held (that is what
/// single-flights it), so a *slow* platform must not be allowed to stall every
/// guarded request for the full client timeout (10s) — beyond this budget the
/// refresh is abandoned, treated as a fetch error (unavailable + backoff), and
/// `failClosed` semantics take over. `/state` is served from a 30s server-side
/// cache, so a healthy platform answers well inside this.
const STATE_REFRESH_BUDGET: Duration = Duration::from_secs(2);

/// A cached provider of the platform's live cost/rate counters, used by the guard
/// middleware to supply `LiveState` to `cost_cap`/`rate_limit` enforcement.
pub struct RemoteLiveState {
    cfg: RemoteConfig,
    cache: Mutex<StateCache>,
    ttl: Duration,
}

impl RemoteLiveState {
    pub fn new(cfg: RemoteConfig) -> Self {
        Self::with_ttl(cfg, STATE_TTL)
    }

    /// Like [`RemoteLiveState::new`] with an explicit snapshot TTL (tests).
    pub fn with_ttl(cfg: RemoteConfig, ttl: Duration) -> Self {
        Self {
            cfg,
            cache: Mutex::new(StateCache {
                snap: None,
                last_error_at: None,
            }),
            ttl,
        }
    }

    /// Return the current live state, revalidating if the cache is older than
    /// the TTL. Sends `If-None-Match`; a `304` just refreshes the snapshot's
    /// timestamp.
    ///
    /// **An expired snapshot that cannot be revalidated is unavailable** —
    /// `None` is returned so `failClosed` policies block and fail-open policies
    /// allow, exactly as they would before any snapshot existed. Serving an
    /// arbitrarily old under-cap snapshot here would let a `/state` outage
    /// silently defeat fail-closed enforcement after warm-up.
    ///
    /// The lock is held across the refresh, which single-flights it: concurrent
    /// callers hitting an expired TTL wait for the one in-flight fetch and then
    /// read its (fresh) result, rather than serving stale data or stampeding
    /// `/state`. After a failed refresh, callers within [`ERROR_BACKOFF`] get
    /// `None` immediately (no per-request connect timeouts during an outage).
    pub async fn get(&self) -> Option<LiveState> {
        let mut guard = self.cache.lock().await;
        if let Some(snap) = &guard.snap {
            if snap.at.elapsed() < self.ttl {
                return Some(snap.state.clone());
            }
        }
        // Snapshot expired (or none yet). If the platform just failed, don't
        // pile on — report unavailable until the backoff lapses.
        if let Some(t) = guard.last_error_at {
            if t.elapsed() < ERROR_BACKOFF {
                return None;
            }
        }

        let prev_etag = guard.snap.as_ref().and_then(|s| s.etag.clone());
        let result = match tokio::time::timeout(
            STATE_REFRESH_BUDGET,
            fetch_state_conditional(&self.cfg, prev_etag.as_deref()),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(format!(
                "state fetch exceeded the {}s in-request refresh budget",
                STATE_REFRESH_BUDGET.as_secs()
            )),
        };
        match result {
            Ok(StateFetch::NotModified) => {
                guard.last_error_at = None;
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
                guard.last_error_at = None;
                if stale {
                    // Counters came from the durable fallback (cache was down);
                    // they're conservative but real, so we still enforce against
                    // them — just make the condition observable.
                    warn!("Nova Guard: live-state served stale (durable fallback)");
                }
                let state = *state;
                guard.snap = Some(StateSnapshot {
                    at: Instant::now(),
                    state: state.clone(),
                    etag,
                });
                Some(state)
            }
            Err(e) => {
                guard.last_error_at = Some(Instant::now());
                warn!(
                    error = %e,
                    "Nova Guard: live-state fetch failed and snapshot is expired; treating as unavailable"
                );
                None
            }
        }
    }
}

/// How long a locally admitted request's estimated cost stays in the pending
/// ledger before it is assumed to have landed in the platform counters
/// (usage-flush interval + `/state` cache TTLs, with margin).
const PENDING_SPEND_TTL: Duration = Duration::from_secs(45);

/// In-process ledger of spend the gateway has admitted but the platform's
/// counters cannot reflect yet (usage is posted asynchronously and `/state` is
/// cached). Admitted requests' *estimated* costs are reserved here and counted
/// on top of the platform spend when evaluating cost caps, closing the window
/// in which a burst of requests all pass an almost-exhausted cap.
///
/// Admission is race-free by construction: [`PendingSpend::reserve`] records
/// this request's estimate and returns the other in-flight reservations in one
/// critical section, *before* the policy evaluation reads them — so two
/// concurrent requests always see each other's reservation, whichever
/// evaluates first. A request that ends up blocked releases its reservation.
///
/// Entries expire after [`PENDING_SPEND_TTL`] (by then the real usage event has
/// been reported and folded into `/state`). Briefly double-counting an entry
/// that already landed only errs toward blocking *near the cap*, which is the
/// correct direction for a hard cap. The running total is maintained
/// incrementally so reads are O(1) amortized (pruning walks only the expired
/// prefix). This is per-process: a true cross-instance guarantee needs an
/// atomic reservation on the platform side.
#[derive(Default)]
pub struct PendingSpend {
    inner: std::sync::Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    /// `(reserved_at, reservation_id, cost)` in insertion (= time) order.
    entries: std::collections::VecDeque<(Instant, u64, f64)>,
    /// Running sum of `entries` costs.
    total: f64,
    next_id: u64,
}

impl PendingInner {
    fn prune(&mut self) {
        while let Some(&(at, _, cost)) = self.entries.front() {
            if at.elapsed() >= PENDING_SPEND_TTL {
                self.total -= cost;
                self.entries.pop_front();
            } else {
                break;
            }
        }
        if self.entries.is_empty() {
            self.total = 0.0; // reset any accumulated float drift
        }
    }
}

impl PendingSpend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically reserve this request's estimated cost and return
    /// `(reservation, other_pending_total)` — the total of every *other*
    /// un-expired reservation, to be folded into the spend the policy engine
    /// evaluates. `reservation` is `None` when the estimate is not positive.
    pub fn reserve(&self, cost_usd: f64) -> (Option<u64>, f64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        let others = inner.total;
        if cost_usd <= 0.0 {
            return (None, others);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        inner.entries.push_back((Instant::now(), id, cost_usd));
        inner.total += cost_usd;
        (Some(id), others)
    }

    /// Release a reservation whose request was NOT forwarded (blocked): it will
    /// consume nothing, so it must stop counting against the cap immediately.
    pub fn release(&self, reservation: u64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        if let Some(idx) = inner
            .entries
            .iter()
            .position(|&(_, id, _)| id == reservation)
        {
            let (_, _, cost) = inner.entries.remove(idx).expect("index just found");
            inner.total -= cost;
            if inner.entries.is_empty() {
                inner.total = 0.0;
            }
        }
    }

    /// Total un-expired pending spend (prunes expired entries; O(1) amortized).
    pub fn sum(&self) -> f64 {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        inner.total
    }
}

/// Result of a conditional `/state` fetch. The state is boxed to keep the
/// variants close in size (`LiveState` carries six maps).
enum StateFetch {
    NotModified,
    Modified {
        state: Box<LiveState>,
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
    // Status first — error bodies may be non-JSON (see fetch_bundle_conditional).
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "state fetch returned {status}: {}",
            truncate_body(&body)
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON ({status}): {e}"))?;
    let stale = json.get("stale").and_then(|s| s.as_bool()).unwrap_or(false);
    Ok(StateFetch::Modified {
        state: Box::new(platform::state_to_live_state(&json)),
        etag,
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_returns_only_other_reservations() {
        let p = PendingSpend::new();
        let (r1, others1) = p.reserve(0.01);
        assert!(r1.is_some());
        assert_eq!(others1, 0.0, "first reservation sees no other spend");
        let (r2, others2) = p.reserve(0.02);
        assert!(r2.is_some());
        assert!((others2 - 0.01).abs() < 1e-12, "second sees the first");
        assert!((p.sum() - 0.03).abs() < 1e-12);
    }

    #[test]
    fn release_removes_a_blocked_reservation() {
        let p = PendingSpend::new();
        let (r1, _) = p.reserve(0.01);
        let (_r2, _) = p.reserve(0.02);
        p.release(r1.unwrap());
        assert!((p.sum() - 0.02).abs() < 1e-12);
        // Releasing twice (or an unknown id) is a no-op.
        p.release(r1.unwrap());
        p.release(999);
        assert!((p.sum() - 0.02).abs() < 1e-12);
    }

    #[test]
    fn non_positive_estimates_are_not_reserved() {
        let p = PendingSpend::new();
        let (r, others) = p.reserve(0.0);
        assert!(r.is_none());
        assert_eq!(others, 0.0);
        assert_eq!(p.sum(), 0.0);
    }
}
