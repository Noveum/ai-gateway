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
//! an event to [`UsageReporter::report`], which enqueues it on a bounded channel;
//! a background task batches (≤ [`MAX_BATCH`]) and flushes on an interval, with
//! `eventId`-idempotent retries for `5xx`/`429`. See the golden rule: one fresh
//! `eventId` per call, reused on every retry, so the server dedups and never
//! double-counts.

use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
const QUEUE_CAPACITY: usize = 10_000;
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
        }
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

/// Handle used by the request path to report usage without blocking on network.
#[derive(Clone)]
pub struct UsageReporter {
    tx: mpsc::Sender<UsageEvent>,
}

impl UsageReporter {
    /// Spawn the background flush task and return a cheap, cloneable handle.
    pub fn spawn(cfg: RemoteConfig) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(flush_loop(cfg, rx, flush_interval()));
        Self { tx }
    }

    /// Enqueue an event. Non-blocking: if the queue is full the event is dropped
    /// with a warning (usage is advisory; the proxy must never stall on it).
    pub fn report(&self, event: UsageEvent) {
        if let Err(e) = self.tx.try_send(event) {
            warn!(error = %e, "Nova Guard: usage queue full/closed; dropping event");
        }
    }
}

/// Background task: drain the channel into batches and flush them.
async fn flush_loop(cfg: RemoteConfig, mut rx: mpsc::Receiver<UsageEvent>, interval: Duration) {
    let mut buf: Vec<UsageEvent> = Vec::with_capacity(MAX_BATCH);
    loop {
        // Block until at least one event arrives (or the channel closes).
        let first = match rx.recv().await {
            Some(e) => e,
            None => break, // all senders dropped → shut down
        };
        buf.clear();
        buf.push(first);

        // Opportunistically coalesce: keep draining what's already queued, then
        // wait up to FLUSH_INTERVAL for a few more, capped at MAX_BATCH.
        while buf.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(e) => buf.push(e),
                Err(_) => break,
            }
        }
        if buf.len() < MAX_BATCH {
            let deadline = tokio::time::sleep(interval);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    maybe = rx.recv() => match maybe {
                        Some(e) => {
                            buf.push(e);
                            if buf.len() >= MAX_BATCH { break; }
                        }
                        None => break, // channel closed; flush what we have and exit after
                    },
                }
            }
        }

        flush_batch(&cfg, &buf).await;
        buf.clear(); // flushed — don't let the shutdown drain re-send this batch
    }
    // Final drain on shutdown: pick up anything that arrived between the last
    // flush and the channel closing. (`recv()` above only returns `None` once the
    // channel is empty, so this is usually a no-op, but it makes shutdown
    // lossless regardless of where the loop broke.)
    while let Ok(e) = rx.try_recv() {
        buf.push(e);
    }
    if !buf.is_empty() {
        flush_batch(&cfg, &buf).await;
    }
}

/// POST one batch with `eventId`-idempotent retries. Best-effort: on exhausted
/// retries or a malformed (`400`) body it logs and drops — reporting failures
/// never propagate to the caller.
async fn flush_batch(cfg: &RemoteConfig, events: &[UsageEvent]) {
    if events.is_empty() {
        return;
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
                    return;
                }
                if status.as_u16() == 429 || status.is_server_error() {
                    // Transient: retry the SAME events (same eventIds → server dedups).
                    let delay = retry_delay(&r, attempt);
                    if attempt == MAX_ATTEMPTS {
                        warn!(%status, count = events.len(), "Nova Guard: usage flush gave up after retries");
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                // 4xx (e.g. 400 malformed, 401/403 auth): resending is futile.
                let body = r.text().await.unwrap_or_default();
                warn!(%status, body = %body, count = events.len(), "Nova Guard: usage flush rejected; dropping batch");
                return;
            }
            Err(e) => {
                if attempt == MAX_ATTEMPTS {
                    warn!(error = %e, count = events.len(), "Nova Guard: usage flush failed after retries");
                    return;
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
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
}
