//! Usage reporting to the Noveum platform NovaGuard API.
//!
//! After every guarded LLM call the gateway reports one usage event to
//! `POST /api/v1/projects/{id}/policies/usage`:
//!
//! * **ALLOWED** — the call ran; carries `costUsd` + token counts so the
//!   platform's rolling cost/rate counters advance (this is what makes cost caps
//!   ever trip: the gateway *reads* `/state` but nothing moves those numbers
//!   unless usage is reported back).
//! * **BLOCKED** — NovaGuard stopped the call (a `cost_cap`/`rate_limit` was
//!   already over the limit); sent *instead* of an ALLOWED event, is not metered,
//!   and triggers the owner "limit hit" email server-side.
//!
//! Reporting is **best-effort and never blocks the request path**: callers hand
//! an event to [`UsageReporter::report`], which appends it to a bounded in-memory
//! queue (evicting the *oldest* event on overflow, see [`QUEUE_CAPACITY`]); a
//! background task batches (≤ [`MAX_BATCH`]) and flushes on an interval, with
//! `eventId`-idempotent retries for `5xx`/`429`. See the golden rule: one fresh
//! `eventId` per call, reused on every retry, so the server dedups and never
//! double-counts.
//!
//! On graceful shutdown the binary calls [`UsageReporter::shutdown`], which
//! drains whatever is still queued under a wall-clock budget so a rolling
//! restart does not silently strand billable records — and cannot hang the
//! process if the platform is unresponsive.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::policy::pricing::CostBreakdown;
use crate::policy::remote::{RemoteConfig, PLATFORM_CLIENT};

/// Max events per `POST` body (the platform accepts a single object or an array
/// up to 500).
const MAX_BATCH: usize = 500;
/// Default time the flush task waits to accumulate a batch before sending.
/// Overridable via `NOVEUM_GUARD_USAGE_FLUSH_MS` (chiefly for tests).
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Resolve the flush interval from the environment, falling back to the default.
fn flush_interval() -> Duration {
    std::env::var("NOVEUM_GUARD_USAGE_FLUSH_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_FLUSH_INTERVAL)
}
/// Bound on the in-memory queue. If the gateway out-runs the platform this far,
/// we drop the oldest events (with a warning) rather than grow unboundedly or
/// stall the hot path — usage reporting is advisory, correctness of the proxy is
/// not.
///
/// Overflow is **drop-oldest** (newest-wins), which is why this is a `VecDeque`
/// behind a mutex and not an `mpsc` channel: `Sender::try_send` rejects the event
/// being offered, i.e. it drops the *newest*. Under the traffic spike that
/// overflows this queue, the newest records are the ones that still describe
/// current spend and are worth keeping; the oldest are the ones most likely to be
/// clamped or superseded server-side anyway (the platform clamps event timestamps
/// to the last 48h).
const QUEUE_CAPACITY: usize = 10_000;
/// Overflow warnings are throttled to the first drop plus one per this many
/// afterwards: a spike that overruns the queue must not turn logging into the
/// next bottleneck. The exact count is always available as a metric
/// ([`UsageReporter::dropped_events`]).
const DROP_LOG_EVERY: u64 = 1_000;
/// Default wall-clock budget for the shutdown flush ([`UsageReporter::shutdown`]).
/// Comfortably inside a typical Kubernetes `terminationGracePeriodSeconds` (30s)
/// so a rolling restart drains queued usage instead of being SIGKILLed mid-flush.
pub const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_secs(5);
/// Retry budget for a transient (`5xx`/`429`/network) flush failure.
const MAX_ATTEMPTS: u32 = 3;
/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
/// Cap on any single backoff wait (also the ceiling for an honored `Retry-After`).
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Server truncates `reason` at 500 chars; do it client-side to avoid a 400.
const MAX_REASON_LEN: usize = 500;

/// One usage event. Serializes to the platform's wire shape; the two outcomes
/// carry different field sets (see [`UsageEvent::allowed`] / [`blocked`]), so all
/// outcome-specific fields are optional and skipped when unset.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEvent {
    pub model: String,
    /// Fresh UUID per call, **reused on retry** (server idempotency key).
    pub event_id: String,
    /// Required on ALLOWED (0..=100000); `0` on BLOCKED (the call never ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u32>,
    /// `"BLOCKED"` marks a block; omitted → server defaults to `"ALLOWED"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    /// Which limit tripped — `"COST_CAP"` | `"RATE_LIMIT"`. Required on BLOCKED.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// ISO-8601 (clamped server-side to the last 48h).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// The pricing catalog version `cost_usd` was computed from, so a billed
    /// amount can be traced back to the exact rate card that produced it.
    /// Without this a historical cost is unreconcilable the moment a rate
    /// changes, and every rate in the catalog changes eventually.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing_version: Option<String>,
    /// The itemized cost: uncached input, cache read, cache write, output and
    /// per-request tool fees, plus whether any dimension could not be priced.
    /// Posted for auditability — an ALLOWED event that says `$0.07` and nothing
    /// else cannot be checked against an invoice, and one whose total quietly
    /// omits an unpriceable dimension cannot be trusted at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_breakdown: Option<CostBreakdown>,
}

impl UsageEvent {
    /// An ALLOWED event for a call that ran. `cost_usd` is clamped to the
    /// platform's accepted range (0..=100000).
    pub fn allowed(
        event_id: String,
        model: impl Into<String>,
        cost_usd: f64,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            event_id,
            cost_usd: Some(cost_usd.clamp(0.0, 100_000.0)),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            request_count: Some(1),
            outcome: None, // defaults to ALLOWED server-side
            blocked_by: None,
            policy_id: None,
            reason: None,
            timestamp: Some(now_iso()),
            pricing_version: None,
            cost_breakdown: None,
        }
    }

    /// An ALLOWED event carrying the full itemized cost.
    ///
    /// Preferred over [`UsageEvent::allowed`] wherever a breakdown exists: the
    /// total is taken from the breakdown (which already charges a conservative
    /// bound for any dimension the catalog could not price), and the components
    /// plus the catalog version ride along so the platform can audit and
    /// reconcile the amount rather than take a bare number on trust.
    pub fn allowed_with_breakdown(
        event_id: String,
        model: impl Into<String>,
        breakdown: CostBreakdown,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Self {
        let mut event = Self::allowed(
            event_id,
            model,
            breakdown.total_usd,
            input_tokens,
            output_tokens,
        );
        event.pricing_version = Some(breakdown.pricing_version.clone());
        event.cost_breakdown = Some(breakdown);
        event
    }

    /// A BLOCKED event sent in place of the model call. `blocked_by` must be one
    /// of the platform's accepted limit types (`COST_CAP`/`RATE_LIMIT`).
    pub fn blocked(
        event_id: String,
        model: impl Into<String>,
        blocked_by: &'static str,
        policy_id: Option<String>,
        reason: Option<String>,
    ) -> Self {
        Self {
            model: model.into(),
            event_id,
            cost_usd: Some(0.0), // never ran
            input_tokens: None,
            output_tokens: None,
            request_count: None,
            outcome: Some("BLOCKED"),
            blocked_by: Some(blocked_by),
            policy_id,
            reason: reason.map(|r| truncate(r, MAX_REASON_LEN)),
            timestamp: Some(now_iso()),
            // A block never ran: there is no cost, so there is nothing to
            // itemize and no rate card to trace an amount back to.
            pricing_version: None,
            cost_breakdown: None,
        }
    }
}

/// Map a gateway policy type (`"cost_cap"`/`"rate_limit"`) to the platform's
/// `blockedBy` enum. Returns `None` for policy types the platform doesn't accept
/// as usage blocks (e.g. text rules like PII/regex) — those must not be reported.
pub fn blocked_by_for(policy_type: &str) -> Option<&'static str> {
    match policy_type {
        "cost_cap" => Some("COST_CAP"),
        "rate_limit" => Some("RATE_LIMIT"),
        _ => None,
    }
}

/// Whether the `n`-th drop should be logged: the first one always (so an
/// operator sees overflow the moment it starts), then one per [`DROP_LOG_EVERY`].
fn should_log_drop(dropped_total: u64) -> bool {
    dropped_total == 1 || dropped_total.is_multiple_of(DROP_LOG_EVERY)
}

/// What happened to an event handed to [`UsageQueue::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enqueued {
    /// Appended with room to spare.
    Queued,
    /// Appended after evicting the oldest queued event (queue was at capacity).
    EvictedOldest,
    /// Not queued at all: the queue is closed (shutting down).
    Rejected,
}

/// Result of a [`UsageReporter::shutdown`] flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushOutcome {
    /// Events the platform acknowledged during this flush.
    pub delivered: u64,
    /// Events the platform refused (4xx) or never accepted (retries exhausted).
    /// Lost — but counted, rather than lost silently.
    pub failed: u64,
    /// Events whose fate is unknown when the flush returned: still queued, plus
    /// any batch that was in flight when the budget expired.
    pub pending: usize,
    /// The budget ran out before the queue drained.
    pub timed_out: bool,
}

/// Bounded FIFO of pending events, shared by the request path (producers) and the
/// flush task (single consumer).
///
/// A plain `Mutex<VecDeque>` rather than a channel precisely because the overflow
/// policy is drop-*oldest*: eviction is an explicit `pop_front` on the full
/// queue. Producers only ever take the lock for a push/pop pair — no I/O, no
/// `await` — so the request path cannot stall on the platform being slow.
struct UsageQueue {
    pending: Mutex<VecDeque<UsageEvent>>,
    capacity: usize,
    /// Events that never reached the platform because they were evicted on
    /// overflow or arrived after shutdown. Monotonic, process-lifetime.
    dropped: AtomicU64,
    /// Events acknowledged by the platform.
    delivered: AtomicU64,
    /// Events the platform rejected or never accepted.
    failed: AtomicU64,
    /// Size of the batch currently being POSTed (already removed from `pending`,
    /// not yet confirmed). Lets a cancelled flush report honestly.
    in_flight: AtomicUsize,
    closed: AtomicBool,
    /// Set when [`UsageReporter::shutdown`] owns the remaining drain, so the
    /// flush task stops taking batches instead of racing it.
    external_drain: AtomicBool,
    /// Signals "an event was queued" to the parked flush task.
    ready: Notify,
    /// Signals "the queue is closing", which also cuts a coalescing wait short.
    closing: Notify,
}

impl UsageQueue {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            pending: Mutex::new(VecDeque::with_capacity(capacity.min(MAX_BATCH))),
            capacity,
            dropped: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            external_drain: AtomicBool::new(false),
            ready: Notify::new(),
            closing: Notify::new(),
        }
    }

    /// Append one event, evicting the oldest if the queue is at capacity.
    /// Never blocks on I/O and never awaits — safe to call from the hot path.
    fn push(&self, event: UsageEvent) -> Enqueued {
        if self.closed.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Enqueued::Rejected;
        }
        let outcome = {
            let mut q = self.pending.lock();
            let evicted = if q.len() >= self.capacity {
                q.pop_front().is_some()
            } else {
                false
            };
            q.push_back(event);
            if evicted {
                Enqueued::EvictedOldest
            } else {
                Enqueued::Queued
            }
        };
        if outcome == Enqueued::EvictedOldest {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.ready.notify_one();
        outcome
    }

    /// Remove up to `max` events, oldest first.
    fn take(&self, max: usize) -> Vec<UsageEvent> {
        let mut q = self.pending.lock();
        let n = q.len().min(max);
        q.drain(..n).collect()
    }

    fn len(&self) -> usize {
        self.pending.lock().len()
    }

    fn is_empty(&self) -> bool {
        self.pending.lock().is_empty()
    }

    /// Queued plus in-flight: everything not yet confirmed delivered or failed.
    fn unconfirmed(&self) -> usize {
        self.len() + self.in_flight.load(Ordering::Relaxed)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Stop accepting events. `external_drain` means a [`UsageReporter::shutdown`]
    /// caller will drain what is left, so the flush task must not race it.
    fn close(&self, external_drain: bool) {
        if external_drain {
            self.external_drain.store(true, Ordering::Release);
        }
        self.closed.store(true, Ordering::Release);
        self.ready.notify_one();
        self.closing.notify_one();
    }
}

/// Handle used by the request path to report usage without blocking on network.
/// Cheap to clone; every clone shares one queue and one flush task.
#[derive(Clone)]
pub struct UsageReporter {
    inner: Arc<ReporterInner>,
}

struct ReporterInner {
    queue: Arc<UsageQueue>,
    cfg: RemoteConfig,
    /// Taken by the first [`UsageReporter::shutdown`] caller so it can wait for
    /// the flush task's in-flight batch before draining the rest itself.
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ReporterInner {
    fn drop(&mut self) {
        // Last handle gone: nothing can enqueue again, so let the flush task
        // retire (it does a best-effort final drain) instead of parking forever
        // on a queue no one can reach.
        self.queue.close(false);
    }
}

impl UsageReporter {
    /// Spawn the background flush task and return a cheap, cloneable handle.
    pub fn spawn(cfg: RemoteConfig) -> Self {
        Self::spawn_with_capacity(cfg, QUEUE_CAPACITY)
    }

    fn spawn_with_capacity(cfg: RemoteConfig, capacity: usize) -> Self {
        let queue = Arc::new(UsageQueue::new(capacity));
        let worker = tokio::spawn(flush_loop(cfg.clone(), queue.clone(), flush_interval()));
        Self {
            inner: Arc::new(ReporterInner {
                queue,
                cfg,
                worker: Mutex::new(Some(worker)),
            }),
        }
    }

    /// Enqueue an event. Non-blocking: if the queue is full the **oldest** queued
    /// event is dropped to make room (usage is advisory; the proxy must never
    /// stall on it, and under overload the freshest records are the ones worth
    /// keeping). Drops are counted — see [`UsageReporter::dropped_events`].
    pub fn report(&self, event: UsageEvent) {
        match self.inner.queue.push(event) {
            Enqueued::Queued => {}
            Enqueued::EvictedOldest => {
                let dropped = self.dropped_events();
                if should_log_drop(dropped) {
                    warn!(
                        dropped_total = dropped,
                        capacity = self.inner.queue.capacity,
                        "Nova Guard: usage queue full; dropping oldest events"
                    );
                }
            }
            Enqueued::Rejected => {
                let dropped = self.dropped_events();
                if should_log_drop(dropped) {
                    warn!(
                        dropped_total = dropped,
                        "Nova Guard: usage reporter shut down; dropping event"
                    );
                }
            }
        }
    }

    /// Metric: events that never reached the platform because the queue
    /// overflowed (oldest evicted) or because they arrived after shutdown.
    /// Monotonic for the life of the process.
    pub fn dropped_events(&self) -> u64 {
        self.inner.queue.dropped.load(Ordering::Relaxed)
    }

    /// Metric: events acknowledged by the platform.
    pub fn delivered_events(&self) -> u64 {
        self.inner.queue.delivered.load(Ordering::Relaxed)
    }

    /// Metric: events the platform rejected or never accepted (retries exhausted).
    pub fn failed_events(&self) -> u64 {
        self.inner.queue.failed.load(Ordering::Relaxed)
    }

    /// Metric (gauge): events currently waiting to be flushed.
    pub fn pending_events(&self) -> usize {
        self.inner.queue.len()
    }

    /// Close the queue and flush what is still in it, giving up after `budget`.
    ///
    /// Called from the graceful-shutdown path so a rolling restart does not
    /// strand billable records. Bounded by construction: the wait for the flush
    /// task's in-flight batch and the drain that follows share one `budget`, so
    /// an unresponsive platform can delay shutdown by at most that long.
    ///
    /// Events reported after this returns are rejected and counted as dropped.
    pub async fn shutdown(&self, budget: Duration) -> FlushOutcome {
        let started = Instant::now();
        let queue = &self.inner.queue;
        queue.close(true);

        // Let the flush task finish the batch it already pulled off the queue —
        // those events are no longer in `pending`, so we cannot re-send them.
        // (Guard is dropped before the await: never hold a lock across `.await`.)
        let worker = self.inner.worker.lock().take();
        if let Some(worker) = worker {
            let _ = tokio::time::timeout(budget, worker).await;
        }

        let remaining = budget.saturating_sub(started.elapsed());
        let cfg = &self.inner.cfg;
        drain_within(queue, remaining, move |batch| async move {
            flush_batch(cfg, &batch).await
        })
        .await
    }
}

/// Drain the queue completely, oldest first, in batches of at most [`MAX_BATCH`],
/// handing each batch to `send` (which reports whether the platform took it).
///
/// Cancellation-safe accounting: a batch is marked in-flight while `send` runs,
/// so if this future is dropped mid-flush the caller can still see that those
/// events are unaccounted for.
async fn drain_pending<F, Fut>(queue: &UsageQueue, send: F)
where
    F: Fn(Vec<UsageEvent>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    loop {
        let batch = queue.take(MAX_BATCH);
        if batch.is_empty() {
            return;
        }
        let n = batch.len();
        queue.in_flight.store(n, Ordering::Relaxed);
        let delivered = send(batch).await;
        queue.in_flight.store(0, Ordering::Relaxed);
        let counter = if delivered {
            &queue.delivered
        } else {
            &queue.failed
        };
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// [`drain_pending`] under a wall-clock budget, reporting what got through.
async fn drain_within<F, Fut>(queue: &UsageQueue, budget: Duration, send: F) -> FlushOutcome
where
    F: Fn(Vec<UsageEvent>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let delivered_before = queue.delivered.load(Ordering::Relaxed);
    let failed_before = queue.failed.load(Ordering::Relaxed);
    let timed_out = tokio::time::timeout(budget, drain_pending(queue, send))
        .await
        .is_err();
    FlushOutcome {
        delivered: queue
            .delivered
            .load(Ordering::Relaxed)
            .saturating_sub(delivered_before),
        failed: queue
            .failed
            .load(Ordering::Relaxed)
            .saturating_sub(failed_before),
        pending: queue.unconfirmed(),
        timed_out,
    }
}

/// Background task: batch the queue and flush it on an interval.
async fn flush_loop(cfg: RemoteConfig, queue: Arc<UsageQueue>, interval: Duration) {
    let cfg = &cfg;
    let send = move |batch: Vec<UsageEvent>| async move { flush_batch(cfg, &batch).await };
    loop {
        if queue.is_closed() {
            break;
        }
        if queue.is_empty() {
            // Park until an event lands or the queue closes. `Notify` stores a
            // permit when there is no waiter, so a push that races this wait is
            // never missed.
            queue.ready.notified().await;
            continue;
        }
        // Opportunistically coalesce: below a full batch, give stragglers one
        // interval to show up — but never make shutdown wait for it.
        if queue.len() < MAX_BATCH {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = queue.closing.notified() => {}
            }
        }
        if queue.external_drain.load(Ordering::Acquire) {
            break; // `shutdown()` owns the rest; don't race it for batches.
        }
        drain_pending(&queue, &send).await;
    }
    // Closed by the last handle going away rather than by `shutdown()`: nobody
    // else will drain, so make a best-effort pass before retiring.
    if !queue.external_drain.load(Ordering::Acquire) {
        drain_pending(&queue, &send).await;
    }
    debug!(
        dropped = queue.dropped.load(Ordering::Relaxed),
        delivered = queue.delivered.load(Ordering::Relaxed),
        "Nova Guard: usage flush task exiting"
    );
}

/// POST one batch with `eventId`-idempotent retries. Best-effort: on exhausted
/// retries or a malformed (`400`) body it logs and drops — reporting failures
/// never propagate to the caller. Returns whether the platform accepted the
/// batch, which is what the delivered/failed counters are built from.
async fn flush_batch(cfg: &RemoteConfig, events: &[UsageEvent]) -> bool {
    if events.is_empty() {
        return true;
    }
    let url = cfg.usage_url();
    for attempt in 1..=MAX_ATTEMPTS {
        let resp = PLATFORM_CLIENT
            .post(&url)
            .bearer_auth(&cfg.api_key)
            .json(events)
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    debug!(count = events.len(), %status, "Nova Guard: usage flushed");
                    return true;
                }
                if status.as_u16() == 429 || status.is_server_error() {
                    // Transient: retry the SAME events (same eventIds → server dedups).
                    let delay = retry_delay(&r, attempt);
                    if attempt == MAX_ATTEMPTS {
                        warn!(%status, count = events.len(), "Nova Guard: usage flush gave up after retries");
                        return false;
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                // 4xx (e.g. 400 malformed, 401/403 auth): resending is futile.
                let body = r.text().await.unwrap_or_default();
                warn!(%status, body = %body, count = events.len(), "Nova Guard: usage flush rejected; dropping batch");
                return false;
            }
            Err(e) => {
                if attempt == MAX_ATTEMPTS {
                    warn!(error = %e, count = events.len(), "Nova Guard: usage flush failed after retries");
                    return false;
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
    false
}

/// Delay before the next retry. Honors `Retry-After` (delta-seconds) on `429`,
/// otherwise exponential backoff. The HTTP-date form of `Retry-After` (RFC 9110
/// §10.2.3) is intentionally not parsed — it falls through to backoff, which is a
/// safe (never-too-long, capped) default for best-effort telemetry.
fn retry_delay(resp: &reqwest::Response, attempt: u32) -> Duration {
    if resp.status().as_u16() == 429 {
        if let Some(secs) = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            return Duration::from_secs(secs).min(MAX_RETRY_DELAY);
        }
    }
    backoff(attempt)
}

/// Exponential backoff: `RETRY_BASE_DELAY * 2^(attempt-1)`, capped.
fn backoff(attempt: u32) -> Duration {
    let mult = 1u32 << (attempt.saturating_sub(1)).min(16);
    (RETRY_BASE_DELAY * mult).min(MAX_RETRY_DELAY)
}

/// ISO-8601 UTC timestamp with a `Z` suffix, second precision.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Truncate on a char boundary so `reason` never exceeds the server's limit.
fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

/// Mint a fresh event id (one per LLM call; reuse on retry).
pub fn new_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allowed_event_shape() {
        let e = UsageEvent::allowed("evt-1".into(), "gpt-4o", 0.0025, 1000, 500);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["eventId"], "evt-1");
        assert_eq!(v["costUsd"], 0.0025);
        assert_eq!(v["inputTokens"], 1000);
        assert_eq!(v["outputTokens"], 500);
        assert_eq!(v["requestCount"], 1);
        // ALLOWED omits outcome/blockedBy so the server applies its default.
        assert!(v.get("outcome").is_none());
        assert!(v.get("blockedBy").is_none());
        assert!(v.get("timestamp").is_some());
    }

    #[test]
    fn blocked_event_shape() {
        let e = UsageEvent::blocked(
            "evt-2".into(),
            "gpt-4o",
            "COST_CAP",
            Some("pol_abc".into()),
            Some("30d cost cap exceeded".into()),
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["outcome"], "BLOCKED");
        assert_eq!(v["blockedBy"], "COST_CAP");
        assert_eq!(v["policyId"], "pol_abc");
        assert_eq!(v["reason"], "30d cost cap exceeded");
        assert_eq!(v["costUsd"], 0.0, "blocked calls never ran → cost 0");
        // BLOCKED omits token counts.
        assert!(v.get("inputTokens").is_none());
    }

    #[test]
    fn cost_is_clamped() {
        let e = UsageEvent::allowed("e".into(), "m", 999_999.0, 0, 0);
        assert_eq!(serde_json::to_value(&e).unwrap()["costUsd"], 100_000.0);
        let neg = UsageEvent::allowed("e".into(), "m", -5.0, 0, 0);
        assert_eq!(serde_json::to_value(&neg).unwrap()["costUsd"], 0.0);
    }

    #[test]
    fn blocked_by_mapping() {
        assert_eq!(blocked_by_for("cost_cap"), Some("COST_CAP"));
        assert_eq!(blocked_by_for("rate_limit"), Some("RATE_LIMIT"));
        // Text-rule blocks are not reportable usage events.
        assert_eq!(blocked_by_for("pii_detection"), None);
        assert_eq!(blocked_by_for("regex_match"), None);
    }

    #[test]
    fn reason_truncated_to_500() {
        let long = "x".repeat(1000);
        let e = UsageEvent::blocked("e".into(), "m", "RATE_LIMIT", None, Some(long));
        let reason = e.reason.unwrap();
        assert_eq!(reason.len(), 500);
    }

    #[test]
    fn batch_serializes_as_array() {
        let batch = vec![
            UsageEvent::allowed("a".into(), "gpt-4o", 0.01, 10, 5),
            UsageEvent::blocked("b".into(), "gpt-4o", "COST_CAP", None, None),
        ];
        let v = serde_json::to_value(&batch).unwrap();
        assert_eq!(
            v,
            json!([
                {"model":"gpt-4o","eventId":"a","costUsd":0.01,"inputTokens":10,"outputTokens":5,"requestCount":1,"timestamp": v[0]["timestamp"]},
                {"model":"gpt-4o","eventId":"b","costUsd":0.0,"outcome":"BLOCKED","blockedBy":"COST_CAP","timestamp": v[1]["timestamp"]},
            ])
        );
    }

    #[test]
    fn event_ids_are_unique() {
        assert_ne!(new_event_id(), new_event_id());
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), RETRY_BASE_DELAY);
        assert_eq!(backoff(2), RETRY_BASE_DELAY * 2);
        assert_eq!(backoff(3), RETRY_BASE_DELAY * 4);
        assert!(backoff(30) <= MAX_RETRY_DELAY);
    }

    // ---- queue overflow / drop accounting -----------------------------------
    //
    // These are deliberately pure (no env, no network, no spawned task) so they
    // cannot race the other tests in this binary.

    fn ev(id: &str) -> UsageEvent {
        UsageEvent::allowed(id.into(), "gpt-4o", 0.001, 10, 5)
    }

    fn ids(events: &[UsageEvent]) -> Vec<String> {
        events.iter().map(|e| e.event_id.clone()).collect()
    }

    #[test]
    fn overflow_drops_the_oldest_and_keeps_the_newest() {
        let q = UsageQueue::new(3);
        assert_eq!(q.push(ev("e1")), Enqueued::Queued);
        assert_eq!(q.push(ev("e2")), Enqueued::Queued);
        assert_eq!(q.push(ev("e3")), Enqueued::Queued);
        // Now at capacity: each further push evicts the front, not the newcomer.
        assert_eq!(q.push(ev("e4")), Enqueued::EvictedOldest);
        assert_eq!(q.push(ev("e5")), Enqueued::EvictedOldest);

        assert_eq!(q.len(), 3, "queue stays bounded");
        assert_eq!(
            ids(&q.take(10)),
            vec!["e3", "e4", "e5"],
            "oldest (e1, e2) dropped; newest retained in FIFO order"
        );
    }

    #[test]
    fn dropped_counter_matches_the_number_evicted() {
        let q = UsageQueue::new(4);
        for i in 0..100 {
            q.push(ev(&format!("e{i}")));
        }
        assert_eq!(q.len(), 4);
        assert_eq!(q.dropped.load(Ordering::Relaxed), 96, "100 pushed - 4 kept");
        // The survivors are the last four.
        assert_eq!(ids(&q.take(10)), vec!["e96", "e97", "e98", "e99"]);
    }

    #[test]
    fn a_queue_under_capacity_drops_nothing() {
        let q = UsageQueue::new(8);
        for i in 0..8 {
            assert_eq!(q.push(ev(&format!("e{i}"))), Enqueued::Queued);
        }
        assert_eq!(q.dropped.load(Ordering::Relaxed), 0);
        assert_eq!(q.take(100).len(), 8);
    }

    #[test]
    fn events_reported_after_close_are_rejected_and_counted() {
        let q = UsageQueue::new(4);
        q.push(ev("before"));
        q.close(true);
        assert_eq!(q.push(ev("after")), Enqueued::Rejected);
        assert_eq!(q.push(ev("later")), Enqueued::Rejected);
        assert_eq!(q.dropped.load(Ordering::Relaxed), 2);
        assert_eq!(
            ids(&q.take(10)),
            vec!["before"],
            "close never discards what is already queued"
        );
    }

    /// Event ids of every batch a test sink was handed, in order.
    type SeenBatches = Arc<Mutex<Vec<Vec<String>>>>;

    /// A sink that records the batches it was handed and accepts them all.
    fn recording_sink() -> (
        SeenBatches,
        impl Fn(Vec<UsageEvent>) -> std::future::Ready<bool>,
    ) {
        let seen: SeenBatches = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            move |batch: Vec<UsageEvent>| {
                seen.lock().push(ids(&batch));
                std::future::ready(true)
            }
        };
        (seen, sink)
    }

    #[tokio::test]
    async fn flush_drains_everything_queued() {
        let q = UsageQueue::new(64);
        for i in 0..10 {
            q.push(ev(&format!("e{i}")));
        }
        let (seen, sink) = recording_sink();

        let outcome = drain_within(&q, Duration::from_secs(5), sink).await;

        assert!(!outcome.timed_out);
        assert_eq!(outcome.delivered, 10);
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.pending, 0, "nothing left behind");
        assert_eq!(q.len(), 0);
        let seen = seen.lock();
        assert_eq!(seen.len(), 1, "one batch under MAX_BATCH");
        assert_eq!(seen[0].len(), 10);
        assert_eq!(seen[0][0], "e0", "oldest first");
        assert_eq!(seen[0][9], "e9");
    }

    #[tokio::test]
    async fn flush_splits_at_max_batch() {
        let q = UsageQueue::new(MAX_BATCH * 2);
        for i in 0..(MAX_BATCH + 5) {
            q.push(ev(&format!("e{i}")));
        }
        let (seen, sink) = recording_sink();

        let outcome = drain_within(&q, Duration::from_secs(5), sink).await;

        assert_eq!(outcome.delivered as usize, MAX_BATCH + 5);
        let seen = seen.lock();
        assert_eq!(
            seen.iter().map(|b| b.len()).collect::<Vec<_>>(),
            vec![MAX_BATCH, 5]
        );
    }

    #[tokio::test]
    async fn a_rejecting_sink_counts_events_as_failed_not_delivered() {
        let q = UsageQueue::new(16);
        for i in 0..3 {
            q.push(ev(&format!("e{i}")));
        }
        let outcome = drain_within(&q, Duration::from_secs(5), |_| std::future::ready(false)).await;
        assert_eq!(outcome.delivered, 0);
        assert_eq!(outcome.failed, 3);
        assert!(!outcome.timed_out);
    }

    /// Shutdown must not be able to hang the process: an unresponsive sink is
    /// abandoned once the budget expires, and the events it swallowed are
    /// reported as unconfirmed rather than as delivered.
    #[tokio::test(start_paused = true)]
    async fn flush_respects_its_timeout_when_the_sink_is_unresponsive() {
        let q = UsageQueue::new(16);
        for i in 0..3 {
            q.push(ev(&format!("e{i}")));
        }
        let started = Instant::now();

        let outcome = drain_within(&q, Duration::from_secs(2), |_| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            true
        })
        .await;

        assert!(outcome.timed_out, "budget must cut the flush short");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "gave up at the budget, not at the sink's own pace: {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.delivered, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(
            outcome.pending, 3,
            "in-flight events are reported as pending"
        );
    }

    /// Points at a closed port: these tests never let the flush task run, so
    /// nothing is ever sent.
    fn test_cfg() -> RemoteConfig {
        RemoteConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "test-key".to_string(),
            project_id: "proj_test".to_string(),
        }
    }

    /// The same drop-oldest guarantee, through the public request-path API.
    /// `#[tokio::test]` is a current-thread runtime and this test never awaits,
    /// so the flush task is not polled and the queue is observed intact.
    #[tokio::test]
    async fn report_evicts_the_oldest_and_exposes_the_drop_count() {
        let reporter = UsageReporter::spawn_with_capacity(test_cfg(), 2);
        for i in 0..5 {
            reporter.report(ev(&format!("e{i}")));
        }
        assert_eq!(reporter.pending_events(), 2, "queue stays bounded");
        assert_eq!(reporter.dropped_events(), 3, "5 reported - 2 retained");
        assert_eq!(
            ids(&reporter.inner.queue.take(10)),
            vec!["e3", "e4"],
            "the newest survive"
        );
    }

    #[test]
    fn drop_logging_is_throttled_but_starts_immediately() {
        assert!(should_log_drop(1), "first drop is always visible");
        assert!(!should_log_drop(2));
        assert!(!should_log_drop(999));
        assert!(should_log_drop(DROP_LOG_EVERY));
        assert!(should_log_drop(DROP_LOG_EVERY * 7));
    }

    #[tokio::test]
    async fn flush_of_an_empty_queue_is_a_no_op() {
        let q = UsageQueue::new(16);
        let outcome = drain_within(&q, Duration::from_secs(5), |_| std::future::ready(true)).await;
        assert_eq!(outcome, FlushOutcome::default());
    }
}
