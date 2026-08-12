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
use tracing::{debug, error, info, warn};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteConfig {
    pub base_url: String,
    pub api_key: String,
    pub project_id: String,
}

/// An explicitly-configured platform bridge that cannot be used as configured.
///
/// Distinct from "not configured": an operator who set one of the two required
/// variables, or set one to an empty value, is *trying* to enable enforcement.
/// Starting a silent pass-through there is the worst outcome — the gateway
/// looks healthy while no cap is in force. Callers surface this and refuse to
/// start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigError {
    pub message: String,
}

impl std::fmt::Display for RemoteConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteConfigError {}

// The env var names the platform bridge is configured with. Defined in the
// shared `platform` module (the Worker validates the same pair) and re-exported
// here so native callers can keep reaching for them via `remote::`.
pub use crate::policy::platform::{API_KEY_VAR, PROJECT_ID_VAR};

impl RemoteConfig {
    /// Build from env. See [`RemoteConfig::from_values`] for the semantics;
    /// this only reads the variables (`None` = unset) and supplies
    /// `NOVEUM_API_URL`.
    pub fn from_env() -> Result<Option<Self>, RemoteConfigError> {
        Self::from_values(
            std::env::var(API_KEY_VAR).ok().as_deref(),
            std::env::var(PROJECT_ID_VAR).ok().as_deref(),
            std::env::var("NOVEUM_API_URL").ok().as_deref(),
        )
    }

    /// Decide what a given pair of configuration values means. Pure, so the
    /// matrix below is testable without mutating process env.
    ///
    /// * neither variable present (`None`) → `Ok(None)`: platform-managed Nova
    ///   Guard is intentionally disabled;
    /// * both present and non-empty → `Ok(Some(config))`;
    /// * exactly one present, or either present but empty/whitespace →
    ///   `Err(RemoteConfigError)`.
    ///
    /// That last case used to be indistinguishable from "disabled", so a
    /// typo'd or half-deployed secret silently started an unguarded
    /// pass-through while the operator believed caps were being enforced.
    pub fn from_values(
        api_key: Option<&str>,
        project_id: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<Option<Self>, RemoteConfigError> {
        let err = |message: String| Err(RemoteConfigError { message });
        let blank = |v: Option<&str>| v.is_some_and(|s| s.trim().is_empty());

        // Present but blank: an unresolved template or a stripped CI variable,
        // never a deliberate choice.
        if blank(api_key) {
            return err(format!(
                "{API_KEY_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide a Noveum service key with `guardrails:read` + `guardrails:ingest`, \
                 or unset both {API_KEY_VAR} and {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }
        if blank(project_id) {
            return err(format!(
                "{PROJECT_ID_VAR} is set but empty; platform-managed Nova Guard cannot start. \
                 Provide the project id to enforce for, or unset both {API_KEY_VAR} and \
                 {PROJECT_ID_VAR} to run without the platform bridge."
            ));
        }

        let (api_key, project_id) = match (api_key, project_id) {
            (None, None) => return Ok(None),
            (Some(k), Some(p)) => (k.trim().to_string(), p.trim().to_string()),
            // Exactly one of the pair: a half-applied configuration.
            (Some(_), None) => {
                return err(format!(
                    "{API_KEY_VAR} is set but {PROJECT_ID_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {PROJECT_ID_VAR}, or unset {API_KEY_VAR} to run \
                     without the platform bridge."
                ))
            }
            (None, Some(_)) => {
                return err(format!(
                    "{PROJECT_ID_VAR} is set but {API_KEY_VAR} is not; platform-managed Nova \
                     Guard needs both. Set {API_KEY_VAR}, or unset {PROJECT_ID_VAR} to run \
                     without the platform bridge."
                ))
            }
        };

        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_API_URL)
            .trim_end_matches('/')
            .to_string();
        Ok(Some(Self {
            base_url,
            api_key,
            project_id,
        }))
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

/// The deliberate override that lets a configured platform bridge start with no
/// policy set at all. Emergency operation only: the platform is unreachable and
/// serving traffic without caps beats serving none.
pub const ALLOW_UNGUARDED_START_VAR: &str = "NOVEUM_GUARD_ALLOW_UNGUARDED_START";

/// Build the startup engine for a configured platform bridge.
///
/// On a successful first fetch, returns the compiled engine and the `ETag` to
/// seed the poller with. When the first fetch **fails** there is no known
/// policy set, so every request would be forwarded unguarded — that is an
/// `Err`, and the caller aborts startup, unless `allow_unguarded_start` opts
/// into it explicitly.
///
/// The old behavior (fall back to the local bundle, else an empty one) is
/// deliberately gone: an operator who configured the platform bridge did not
/// ask for whatever happens to be on disk, and an empty bundle enforces
/// nothing while looking perfectly healthy.
pub async fn bootstrap_engine(
    cfg: &RemoteConfig,
    opts: crate::policy::engine::EngineOptions,
    allow_unguarded_start: bool,
) -> Result<(PolicyEngine, Option<String>), String> {
    match fetch_bundle_with_etag(cfg).await {
        Ok((bundle, etag)) => Ok((PolicyEngine::from_bundle(&bundle, opts), etag)),
        Err(e) if allow_unguarded_start => {
            // ERROR, not WARN: the gateway is up and enforcing nothing. This
            // line is the only signal that a cap an operator believes is live
            // is not, so it must clear any WARN-level log filter.
            error!(
                error = %e, override_var = ALLOW_UNGUARDED_START_VAR,
                "Nova Guard: initial platform policy fetch FAILED and the unguarded-start override \
                 is set; serving traffic with NO enforcement until a poll succeeds"
            );
            Ok((
                PolicyEngine::from_bundle(&PolicyBundle::default(), opts),
                None,
            ))
        }
        Err(e) => Err(format!(
            "the initial platform policy fetch from {} failed ({e}), so no policy set is known and \
             every request would be forwarded unguarded. Startup is aborted deliberately. Fix \
             connectivity/credentials and restart, or set {ALLOW_UNGUARDED_START_VAR}=true to \
             start unguarded anyway (emergency use only — caps and fail-closed policies will NOT \
             be enforced until a later poll succeeds).",
            cfg.base_url
        )),
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

/// How long a COMPLETED request's reservation stays in the pending ledger
/// before it is assumed to have landed in the platform counters (usage-flush
/// interval + `/state` cache TTLs, with margin). The clock starts at request
/// completion, not admission — an active request never loses its reservation.
const PENDING_SPEND_TTL: Duration = Duration::from_secs(45);

/// Backstop for reservations whose completion guard never fires (which should
/// not happen — the guard is RAII on the response body — but a leak here must
/// not poison admission forever). Far above any real request duration.
const ACTIVE_RESERVATION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Aggregate of the *other* in-flight/pending reservations, folded into the
/// live counters before policy evaluation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PendingTotals {
    pub cost_usd: f64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Copy)]
struct PendingEntry {
    id: u64,
    at: Instant,
    cost_usd: f64,
    tokens: u64,
}

/// In-process ledger of usage the gateway has admitted but the platform's
/// counters cannot reflect yet (usage is posted asynchronously and `/state` is
/// cached). Each admitted request reserves its *estimated* cost, its request
/// count, and its estimated tokens; the totals are counted on top of the
/// platform counters when evaluating `cost_cap` AND `rate_limit`, closing the
/// window in which a burst of requests all pass an almost-exhausted limit.
///
/// Admission is race-free by construction: [`PendingSpend::reserve`] records
/// this request's reservation and returns the other reservations' totals in
/// one critical section, *before* the policy evaluation reads them — so two
/// concurrent requests always see each other's reservation, whichever
/// evaluates first. A request that ends up blocked releases its reservation.
///
/// Reservation lifecycle: a reservation is **active** (never expires — a
/// long-lived or streaming request keeps its cap protection for its whole
/// duration) until [`PendingSpend::complete`] fires — via the RAII
/// [`ReservationGuard`] attached to the response body, which also covers
/// client-cancellation — and then ages out [`PENDING_SPEND_TTL`] after
/// completion, by which time the real usage event has been reported and folded
/// into `/state`. Briefly double-counting a completed entry that already
/// landed only errs toward blocking *near the limit*, the correct direction
/// for a hard cap. Totals are maintained incrementally (O(1) amortized reads;
/// completed-entry pruning walks only the expired prefix, active entries are
/// bounded by in-flight concurrency). This is per-process: a true
/// cross-instance guarantee needs an atomic reservation on the platform side.
#[derive(Default)]
pub struct PendingSpend {
    inner: std::sync::Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    /// Reservations for requests still in flight (`at` = admission time).
    active: Vec<PendingEntry>,
    /// Reservations for completed requests (`at` = completion time), in
    /// completion order so pruning pops the expired prefix.
    completed: std::collections::VecDeque<PendingEntry>,
    /// Running totals over `active` + `completed`.
    totals: PendingTotals,
    next_id: u64,
}

impl PendingInner {
    fn subtract(totals: &mut PendingTotals, e: &PendingEntry) {
        totals.cost_usd -= e.cost_usd;
        totals.requests = totals.requests.saturating_sub(1);
        totals.tokens = totals.tokens.saturating_sub(e.tokens);
    }

    fn prune(&mut self) {
        while let Some(front) = self.completed.front() {
            if front.at.elapsed() >= PENDING_SPEND_TTL {
                let e = self.completed.pop_front().expect("front just checked");
                Self::subtract(&mut self.totals, &e);
            } else {
                break;
            }
        }
        // Backstop: reap active entries whose guard never fired. `active` is
        // bounded by in-flight concurrency, so the scan is cheap.
        let mut totals = self.totals;
        self.active.retain(|e| {
            let leaked = e.at.elapsed() >= ACTIVE_RESERVATION_MAX_AGE;
            if leaked {
                warn!(
                    reservation = e.id,
                    "Nova Guard: reaping leaked active reservation"
                );
                Self::subtract(&mut totals, e);
            }
            !leaked
        });
        self.totals = totals;
        if self.active.is_empty() && self.completed.is_empty() {
            self.totals = PendingTotals::default(); // reset any float drift
        }
    }
}

impl PendingSpend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically reserve this request's estimated usage (cost, one request,
    /// estimated tokens) and return `(reservation_id, other_pending_totals)` —
    /// the totals of every *other* un-expired reservation, to be folded into
    /// the counters the policy engine evaluates.
    pub fn reserve(&self, cost_usd: f64, tokens: u64) -> (u64, PendingTotals) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        let others = inner.totals;
        let id = inner.next_id;
        inner.next_id += 1;
        inner.active.push(PendingEntry {
            id,
            at: Instant::now(),
            cost_usd: cost_usd.max(0.0),
            tokens,
        });
        inner.totals.cost_usd += cost_usd.max(0.0);
        inner.totals.requests += 1;
        inner.totals.tokens += tokens;
        (id, others)
    }

    /// Release a reservation whose request was NOT forwarded (blocked): it
    /// will consume nothing, so it must stop counting immediately.
    pub fn release(&self, reservation: u64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        if let Some(idx) = inner.active.iter().position(|e| e.id == reservation) {
            let e = inner.active.swap_remove(idx);
            let mut t = inner.totals;
            PendingInner::subtract(&mut t, &e);
            inner.totals = t;
            if inner.active.is_empty() && inner.completed.is_empty() {
                inner.totals = PendingTotals::default();
            }
        }
    }

    /// Mark a forwarded request as completed: its reservation keeps counting
    /// for [`PENDING_SPEND_TTL`] from NOW (covering the usage-report + state
    /// ingestion lag), then expires.
    pub fn complete(&self, reservation: u64) {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        if let Some(idx) = inner.active.iter().position(|e| e.id == reservation) {
            let mut e = inner.active.swap_remove(idx);
            e.at = Instant::now();
            inner.completed.push_back(e);
        }
    }

    /// How many reservations are still ACTIVE (admitted, not yet completed or
    /// released). Anything left here after a request has ended is a leak that
    /// would keep counting against the cap until the 15-minute backstop, so
    /// cancellation tests assert on this rather than on the totals (a
    /// *completed* entry legitimately keeps counting for its short TTL).
    pub fn active_count(&self) -> usize {
        let inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.active.len()
    }

    /// Current un-expired pending totals (prunes expired entries).
    pub fn sum(&self) -> PendingTotals {
        let mut inner = self.inner.lock().expect("pending-spend lock poisoned");
        inner.prune();
        inner.totals
    }
}

/// RAII completion guard for an admitted request's reservation.
///
/// Created the instant [`PendingSpend::reserve`] returns — *not* once a
/// response exists — and then moved along the request: held by the middleware
/// future while it awaits upstream response headers, and finally transferred
/// into the outgoing response body. Whenever and wherever it is dropped, the
/// reservation transitions from *active* to *completed* and starts its
/// post-completion TTL:
///
/// - normal completion: the response body finished streaming;
/// - client disconnect mid-body: the wrapped body stream is dropped;
/// - **cancellation before upstream headers**: the middleware future itself is
///   dropped, taking the guard with it. This is the case a response-only guard
///   missed — the entry stayed ACTIVE until the 15-minute leak backstop and
///   kept blocking a `maxRequests: 1` policy long after the client had gone.
///
/// A request that never reaches the provider (blocked at the input phase) calls
/// [`ReservationGuard::release`] instead, dropping the reservation outright.
pub struct ReservationGuard {
    ledger: Arc<PendingSpend>,
    id: u64,
    /// Cleared by [`release`](Self::release) so `Drop` becomes a no-op.
    armed: bool,
}

impl ReservationGuard {
    pub fn new(ledger: Arc<PendingSpend>, id: u64) -> Self {
        Self {
            ledger,
            id,
            armed: true,
        }
    }

    /// The guarded reservation id (diagnostics/tests).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Give the reservation back: the request was blocked before forwarding, so
    /// it will consume nothing and must stop counting immediately rather than
    /// linger for the post-completion TTL.
    pub fn release(mut self) {
        self.armed = false;
        self.ledger.release(self.id);
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ledger.complete(self.id);
        }
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

    /// The full configuration matrix from the review's §4.5. `None` is an unset
    /// variable; `Some("")`/`Some("  ")` is one that is set but empty.
    #[test]
    fn remote_config_distinguishes_disabled_from_misconfigured() {
        // Neither set: platform-managed Nova Guard is off, and that is fine.
        assert_eq!(RemoteConfig::from_values(None, None, None), Ok(None));

        // Both set: configured.
        let cfg = RemoteConfig::from_values(Some("sk-test"), Some("proj_1"), None)
            .unwrap()
            .expect("both values present must configure the bridge");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.project_id, "proj_1");
        assert_eq!(cfg.base_url, DEFAULT_API_URL);

        // Exactly one set: a half-applied config must NOT start pass-through.
        for (key, project, expect_names) in [
            (Some("sk-test"), None, API_KEY_VAR),
            (None, Some("proj_1"), PROJECT_ID_VAR),
        ] {
            let e = RemoteConfig::from_values(key, project, None)
                .expect_err("exactly one variable must be a configuration error");
            assert!(
                e.message.contains(expect_names) && e.message.contains("needs both"),
                "unhelpful message: {}",
                e.message
            );
        }

        // Set but empty/whitespace: a broken secret, not a choice.
        for (key, project, culprit) in [
            (Some(""), Some("proj_1"), API_KEY_VAR),
            (Some("   "), Some("proj_1"), API_KEY_VAR),
            (Some("sk-test"), Some(""), PROJECT_ID_VAR),
            (Some("sk-test"), Some("\t "), PROJECT_ID_VAR),
            (Some(""), Some(""), API_KEY_VAR),
            // Empty on one side and absent on the other is still explicit.
            (Some(""), None, API_KEY_VAR),
            (None, Some(""), PROJECT_ID_VAR),
        ] {
            let e = RemoteConfig::from_values(key, project, None)
                .expect_err("an empty value must be a configuration error");
            assert!(
                e.message.contains(culprit) && e.message.contains("empty"),
                "unhelpful message for ({key:?}, {project:?}): {}",
                e.message
            );
        }
    }

    #[test]
    fn remote_config_normalizes_values_and_base_url() {
        // Surrounding whitespace on a real value is trimmed, not treated as
        // part of the key (a common copy-paste / `echo` artifact in secrets).
        let cfg = RemoteConfig::from_values(
            Some(" sk-test\n"),
            Some(" proj_1 "),
            Some("  https://api.example.com/  "),
        )
        .unwrap()
        .unwrap();
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.project_id, "proj_1");
        // Trailing slashes are stripped so the URL builders can't double up.
        assert_eq!(cfg.base_url, "https://api.example.com");
        assert_eq!(
            cfg.policies_url(),
            "https://api.example.com/api/v1/projects/proj_1/policies/effective"
        );
        // An unset or blank base URL falls back to the default host.
        for base in [None, Some(""), Some("   ")] {
            let c = RemoteConfig::from_values(Some("k"), Some("p"), base)
                .unwrap()
                .unwrap();
            assert_eq!(c.base_url, DEFAULT_API_URL, "base={base:?}");
        }
    }

    #[test]
    fn reserve_returns_only_other_reservations() {
        let p = PendingSpend::new();
        let (_r1, others1) = p.reserve(0.01, 100);
        assert_eq!(others1, PendingTotals::default(), "first sees nothing");
        let (_r2, others2) = p.reserve(0.02, 50);
        assert!(
            (others2.cost_usd - 0.01).abs() < 1e-12,
            "second sees the first"
        );
        assert_eq!(others2.requests, 1);
        assert_eq!(others2.tokens, 100);
        let sum = p.sum();
        assert!((sum.cost_usd - 0.03).abs() < 1e-12);
        assert_eq!(sum.requests, 2);
        assert_eq!(sum.tokens, 150);
    }

    #[test]
    fn zero_cost_requests_still_reserve_request_and_token_capacity() {
        // An unpriced model can't reserve cost, but its request count and
        // tokens must still count toward rate limits.
        let p = PendingSpend::new();
        let (_r, _) = p.reserve(0.0, 42);
        let sum = p.sum();
        assert_eq!(sum.cost_usd, 0.0);
        assert_eq!(sum.requests, 1);
        assert_eq!(sum.tokens, 42);
    }

    #[test]
    fn release_removes_a_blocked_reservation() {
        let p = PendingSpend::new();
        let (r1, _) = p.reserve(0.01, 10);
        let (_r2, _) = p.reserve(0.02, 20);
        p.release(r1);
        let sum = p.sum();
        assert!((sum.cost_usd - 0.02).abs() < 1e-12);
        assert_eq!(sum.requests, 1);
        assert_eq!(sum.tokens, 20);
        // Releasing twice (or an unknown id) is a no-op.
        p.release(r1);
        p.release(999);
        assert_eq!(p.sum().requests, 1);
    }

    #[test]
    fn active_reservations_do_not_expire_and_complete_starts_the_ttl() {
        let p = PendingSpend::new();
        let (r, _) = p.reserve(0.01, 10);
        // Active entries never age out via the completed-prefix pruning; the
        // reservation is still counted regardless of admission age (the
        // 15-minute leak backstop is not reachable in a unit test).
        assert_eq!(p.sum().requests, 1);
        // Completing moves it to the TTL'd set — still counted immediately
        // after completion (the usage hasn't landed in /state yet).
        p.complete(r);
        assert_eq!(p.sum().requests, 1, "completed entries count until TTL");
        // Completing or releasing again is a no-op.
        p.complete(r);
        p.release(r);
        assert_eq!(p.sum().requests, 1);
    }

    #[test]
    fn reservation_guard_completes_on_drop() {
        let ledger = Arc::new(PendingSpend::new());
        let (r, _) = ledger.reserve(0.01, 10);
        {
            let _guard = ReservationGuard::new(ledger.clone(), r);
        } // dropped here → complete()
          // Entry moved to completed (still counted within TTL) — and is no
          // longer releasable, proving it left the active set.
        ledger.release(r);
        assert_eq!(ledger.sum().requests, 1);
    }
}
