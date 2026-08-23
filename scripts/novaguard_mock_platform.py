#!/usr/bin/env python3
"""A tiny stand-in for the Noveum platform NovaGuard API, for local end-to-end
testing of the ai-gateway platform bridge WITHOUT the real backend.

Implements the project-scoped endpoints the gateway calls:

  GET  /api/v1/projects/{id}/policies/effective   -> the policy set (+ ETag/304)
  GET  /api/v1/projects/{id}/policies/state        -> live cost/rate counters
  POST /api/v1/projects/{id}/policies/usage        -> 202; ALLOWED events add to
                                                      the cost counter (so caps
                                                      climb and eventually trip)
  POST /api/v1/projects/{id}/policies/admit        -> atomic admission. Allows
                                                      until the reserved total
                                                      would cross the cap, then
                                                      blocks. NOTE a block is
                                                      HTTP 200 {allowed:false}.
  POST /api/v1/projects/{id}/policies/reservations/{rid}/complete -> 202,
                                                      replaces the estimate with
                                                      the real usage
  POST /api/v1/projects/{id}/policies/reservations/{rid}/abandon  -> 202, keeps
                                                      the estimate applied
  POST /api/v1/projects/{id}/policies/reservations/{rid}/cancel   -> 202,
                                                      releases the hold

plus mock OpenAI and Anthropic providers (so the ALLOWED paths can be exercised
hermetically, with the gateway's provider base-URL overrides pointed here):

  POST /v1/chat/completions                        -> 200 chat completion with a
                                                      configurable usage block,
                                                      or, for `"stream": true`,
                                                      an OpenAI-shaped SSE stream
                                                      ending in a usage frame
  POST /v1/messages                                -> native Anthropic response
                                                      or SSE, including tools

Config via env:
  MOCK_PORT              (default 8787)
  MOCK_MAX_USD           cost_cap maxUsd            (default 0.01)
  MOCK_POLICY_TYPE       COST_CAP | RATE_LIMIT       (default COST_CAP)
  MOCK_MAX_REQUESTS      rate_limit maxRequests      (default 1)
  MOCK_WINDOW            cost_cap window            (default 1d_rolling)
  MOCK_SEED_USD          initial cost in the window (default 0.0)
  MOCK_FAIL_CLOSED       policy failClosed          (default true)
  MOCK_EVENTS_FILE       append every received usage event as JSONL here
  MOCK_PROMPT_TOKENS     mock provider prompt_tokens     (default 100000)
  MOCK_COMPLETION_TOKENS mock provider completion_tokens (default 100000)
  MOCK_POLICY_SOURCE     policy source: project | org    (default project)
  MOCK_ORG_STATE         serve the nested `org` block in /state (default 1).
                         Set 0 to emulate an older platform: an org-scoped
                         policy must then take the unavailable-state path
                         rather than reading the project's counters.
  MOCK_ORG_OTHER_USD     spend already on the org's books from OTHER projects,
                         so an org cap trips on combined usage (default 0.0)
  MOCK_STATE_UNAVAILABLE /state answers 503 instead of zeroed counters, to
                         exercise the total-storage-outage path (default 0)
  MOCK_ADMIT_UNAVAILABLE /admit answers 503, to prove a 503 is never an allow
                         (default 0)
  MOCK_STREAM_DELAY_MS   pause between SSE frames, so the frames arrive as
                         separate transport chunks and the tee is exercised
                         across chunk boundaries (default 40)
  MOCK_STREAM_PROMPT_TOKENS      usage in the terminal SSE frame (default 11)
  MOCK_STREAM_COMPLETION_TOKENS  usage in the terminal SSE frame (default 4)
  MOCK_STREAM_NO_USAGE   omit the terminal usage frame, so the stream ends
                         without authoritative usage and must ABANDON (default 0)
  MOCK_NO_POLICIES       return an empty effective policy set (default 0)
  MOCK_POLICY_MODE       explicit policy mode: ENFORCE | SHADOW | OFF
                         (default absent/null, which the platform treats as enforce)
  MOCK_ENFORCEMENT_MODE  cost_cap enforcement: STRICT | ADVISORY
                         (default absent/null, using the in-process ledger)
  MOCK_SCOPE_TO_MODELS   comma-separated cost-cap model scope (default absent)
  MOCK_EXPAND_REDACT_WITH_LEN
                         when positive, add an input REGEX_MATCH policy that
                         replaces the literal `x` with this many `Y` bytes;
                         used to prove admission measures the transformed body
  MOCK_RESERVATION_LEASE_MS
                         active-reservation lifetime before the mock admission
                         service may reap it (default 900000, matching 15m)
  MOCK_PROVIDER_DELAY_MS hold each mock provider request open for this many ms,
                         making concurrent-client overlap observable (default 0)
  MOCK_POLICY_RACE       make the first effective-policy response an old empty
                         bundle after a delay and the second a newer blocking
                         bundle immediately, for deterministic cache-race tests
  MOCK_POLICY_RACE_OLD_DELAY_MS
                         delay for the old response above (default 500)

Every request is logged to stderr; received usage events are printed (and, when
MOCK_EVENTS_FILE is set, appended as JSON lines so a test harness can assert on
them). Bearer auth is accepted but not verified (any token passes) — this is a
test double, not the real auth plane.
"""
import hashlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("MOCK_PORT", "8787"))
MAX_USD = float(os.environ.get("MOCK_MAX_USD", "0.01"))
POLICY_TYPE = os.environ.get("MOCK_POLICY_TYPE", "COST_CAP").strip().upper()
if POLICY_TYPE not in ("COST_CAP", "RATE_LIMIT"):
    raise ValueError("MOCK_POLICY_TYPE must be COST_CAP or RATE_LIMIT")
MAX_REQUESTS = int(os.environ.get("MOCK_MAX_REQUESTS", "1"))
if MAX_REQUESTS <= 0:
    raise ValueError("MOCK_MAX_REQUESTS must be positive")
WINDOW = os.environ.get("MOCK_WINDOW", "1d_rolling")
FAIL_CLOSED = os.environ.get("MOCK_FAIL_CLOSED", "true").lower() in ("1", "true", "yes")
EVENTS_FILE = os.environ.get("MOCK_EVENTS_FILE", "")
PROMPT_TOKENS = int(os.environ.get("MOCK_PROMPT_TOKENS", "100000"))
COMPLETION_TOKENS = int(os.environ.get("MOCK_COMPLETION_TOKENS", "100000"))

POLICY_SOURCE = os.environ.get("MOCK_POLICY_SOURCE", "project")
# Serve the nested organization block in /state (the real backend now does).
# Set to "0" to emulate an older platform that ships project counters only —
# an org-scoped policy must then take the unavailable-state path, NOT silently
# read the project's counters.
SERVE_ORG_STATE = os.environ.get("MOCK_ORG_STATE", "1").lower() in ("1", "true", "yes")
# Emulate a total storage outage: /state answers 503, never zeroed counters.
STATE_UNAVAILABLE = os.environ.get("MOCK_STATE_UNAVAILABLE", "0").lower() in ("1", "true", "yes")
# Spend already on the organization's books from OTHER projects, so an org cap
# can be tripped by combined usage rather than this project's alone.
ORG_OTHER_USD = float(os.environ.get("MOCK_ORG_OTHER_USD", "0.0"))
# Emulate the admission service being unevaluable. A 503 must never be read as
# an allow; `failClosed` then decides.
ADMIT_UNAVAILABLE = os.environ.get("MOCK_ADMIT_UNAVAILABLE", "0").lower() in ("1", "true", "yes")
STREAM_DELAY_MS = int(os.environ.get("MOCK_STREAM_DELAY_MS", "40"))
STREAM_PROMPT_TOKENS = int(os.environ.get("MOCK_STREAM_PROMPT_TOKENS", "11"))
STREAM_COMPLETION_TOKENS = int(os.environ.get("MOCK_STREAM_COMPLETION_TOKENS", "4"))
STREAM_NO_USAGE = os.environ.get("MOCK_STREAM_NO_USAGE", "0").lower() in ("1", "true", "yes")
NO_POLICIES = os.environ.get("MOCK_NO_POLICIES", "0").lower() in ("1", "true", "yes")
PROVIDER_DELAY_MS = int(os.environ.get("MOCK_PROVIDER_DELAY_MS", "0"))
POLICY_MODE = os.environ.get("MOCK_POLICY_MODE", "").strip().upper() or None
POLICY_RACE = os.environ.get("MOCK_POLICY_RACE", "0").lower() in ("1", "true", "yes")
POLICY_RACE_OLD_DELAY_MS = int(os.environ.get("MOCK_POLICY_RACE_OLD_DELAY_MS", "500"))
SCOPE_TO_MODELS = [
    model.strip()
    for model in os.environ.get("MOCK_SCOPE_TO_MODELS", "").split(",")
    if model.strip()
]
EXPAND_REDACT_WITH_LEN = int(os.environ.get("MOCK_EXPAND_REDACT_WITH_LEN", "0"))
if EXPAND_REDACT_WITH_LEN < 0:
    raise ValueError("MOCK_EXPAND_REDACT_WITH_LEN must be non-negative")
RESERVATION_LEASE_MS = int(os.environ.get("MOCK_RESERVATION_LEASE_MS", "900000"))
if RESERVATION_LEASE_MS <= 0:
    raise ValueError("MOCK_RESERVATION_LEASE_MS must be positive")
# cost_cap `enforcementMode`. STRICT selects platform-atomic admission, which is
# also what makes an unavailable /admit fail CLOSED; unset/ADVISORY leaves the
# cap on the in-process ledger, where a 503 from /admit is logged and the request
# proceeds (the cap is still evaluated against live /state).
ENFORCEMENT_MODE = os.environ.get("MOCK_ENFORCEMENT_MODE", "").strip().upper()

_lock = threading.Lock()
_stats_lock = threading.Lock()

# Request-level counters are deliberately separate from the mutable spend and
# reservation state below. The Worker concurrency phase reads these through the
# hermetic /__mock/stats endpoint after every waitUntil settlement has landed,
# which lets it assert exact cardinality rather than infer success from one log
# substring among interleaved request threads.
_stats = {
    "admitRequests": 0,
    "admitNewAllowed": 0,
    "admitBlocked": 0,
    "admitReplayed": 0,
    "reservationReaped": 0,
    "providerOpenaiBuffered": 0,
    "providerOpenaiStreaming": 0,
    "providerAnthropicBuffered": 0,
    "providerAnthropicStreaming": 0,
    "providerInFlight": 0,
    "providerMaxInFlight": 0,
    "settlementRequests": 0,
    "settlementCompleted": 0,
    "settlementAbandoned": 0,
    "settlementCancelled": 0,
    "settlementIdempotent": 0,
    "httpErrors": 0,
    "policyFetchRequests": 0,
}

def _empty_scope():
    return {
        "cost": {"1d_rolling": 0.0, "7d_rolling": 0.0, "30d_rolling": 0.0, "1mo_calendar": 0.0, "perModel": {}},
        "rate": {"requests_1m": 0, "requests_1h": 0, "requests_1d": 0, "tokens_1m": 0, "tokens_1h": 0, "tokens_1d": 0},
    }

# Live, mutable state. ALLOWED usage events accumulate into `cost` so a cost cap
# climbs and trips exactly like the real durable counters. Every event lands in
# BOTH scopes, matching the backend's dual-prefix write.
_state = _empty_scope()
_org_state = _empty_scope()
_seed = float(os.environ.get("MOCK_SEED_USD", "0.0"))
for w in ("1d_rolling", "7d_rolling", "30d_rolling", "1mo_calendar"):
    _state["cost"][w] = _seed
    _org_state["cost"][w] = _seed + ORG_OTHER_USD

COST_CAP_CONFIG = dict(
    {"window": WINDOW, "maxUsd": MAX_USD, "action": "BLOCK"},
    **({"enforcementMode": ENFORCEMENT_MODE} if ENFORCEMENT_MODE else {}),
    **({"scopeToModels": SCOPE_TO_MODELS} if SCOPE_TO_MODELS else {}),
)

if POLICY_TYPE == "RATE_LIMIT":
    PRIMARY_POLICY = {
        "policyId": "pol_mock_rate_limit",
        "name": "E2E request rate limit",
        "type": "RATE_LIMIT",
        "enabled": True,
        "failClosed": FAIL_CLOSED,
        "mode": POLICY_MODE,
        "priority": 10,
        "source": POLICY_SOURCE,
        "config": {
            "windows": [{
                "period": "1m",
                "maxRequests": MAX_REQUESTS,
                "action": "BLOCK",
            }],
        },
    }
else:
    PRIMARY_POLICY = {
        "policyId": "pol_mock_cost_cap",
        "name": "E2E daily cost cap",
        "type": "COST_CAP",
        "enabled": True,
        "failClosed": FAIL_CLOSED,
        # Deprecated/null on the real backend; gateway should still ENFORCE.
        # The env switch exists only to exercise explicit SHADOW/OFF payloads.
        "mode": POLICY_MODE,
        "priority": 10,
        "source": POLICY_SOURCE,
        "config": COST_CAP_CONFIG,
    }

POLICIES = {"policies": [PRIMARY_POLICY]}
if EXPAND_REDACT_WITH_LEN:
    POLICIES["policies"].append({
        "policyId": "pol_mock_expand_input",
        "name": "E2E expand input before admission",
        "type": "REGEX_MATCH",
        "enabled": True,
        "failClosed": True,
        "mode": "ENFORCE",
        "priority": 5,
        "source": POLICY_SOURCE,
        "config": {
            "phase": "input",
            "patterns": [{"name": "expand-x", "regex": "x"}],
            "action": "REDACT",
            "redactWith": "Y" * EXPAND_REDACT_WITH_LEN,
        },
    })
if NO_POLICIES:
    POLICIES = {"policies": []}
# Derived from the policy CONTENT, not a constant. A fixed ETag makes the mock
# answer 304 after a knob changes the policy set, so a gateway that already
# fetched once keeps enforcing the previous configuration and the test silently
# measures the wrong thing.
POLICIES_ETAG = '"mock-%s"' % hashlib.sha1(
    json.dumps(POLICIES, sort_keys=True).encode()
).hexdigest()[:16]

POLICY_RACE_OLD = {"policies": []}
POLICY_RACE_NEW = {
    "policies": [{
        "policyId": "pol_cache_race_new",
        "name": "E2E cache race winner",
        "type": "REGEX_MATCH",
        "enabled": True,
        "failClosed": True,
        "mode": "ENFORCE",
        "priority": 1,
        "source": "project",
        "config": {
            "phase": "input",
            "patterns": [{"name": "race-marker", "regex": "race-block-me"}],
            "action": "BLOCK",
        },
    }]
}
POLICY_RACE_OLD_ETAG = '"cache-race-old"'
POLICY_RACE_NEW_ETAG = '"cache-race-new"'

# --- Reservations ----------------------------------------------------------
#
# The real admission API holds ONE authoritative counter, so a reservation is
# visible to every replica the moment it is taken. That is the whole point of
# strict mode, and this double reproduces it the same way: reserved amounts are
# added to the spend the cap is evaluated against, and settlement replaces the
# estimate with the truth (complete), keeps it (abandon) or releases it (cancel).
#
# `_reservations` maps reservationId -> {"requestId", "costUsd", "state"}.
# `_by_request` maps requestId -> reservationId, which is what makes a retry
# REPLAY rather than reserve twice.
_reservations = {}
_by_request = {}
_reservation_seq = 0
# Sum of the estimates currently held. Folded into the /admit cap check, and
# deliberately NOT into /state: the real platform reports settled counters
# there, and holding the reservation set privately is exactly what makes the
# cap atomic for every replica rather than something each one re-derives.
_reserved_usd = 0.0
# Every settlement, in order, so a harness can assert on the sequence.
_settlements = []
# Every admission payload, in arrival order. These bodies contain only metering
# metadata (never provider credentials or prompt text) and let the Worker E2E
# assert the exact estimate sent across the platform boundary.
_admit_bodies = []
# The transformed-body phase compares the hold with the exact bytes Workerd
# forwarded. This capture is intentionally exposed only by this hermetic test
# double; production requests never pass through this script.
_provider_openai_requests = []


def log(*a):
    print("[mock-platform]", *a, file=sys.stderr, flush=True)


def bump_stat(name):
    with _stats_lock:
        _stats[name] += 1
        return _stats[name]


def provider_started():
    with _stats_lock:
        _stats["providerInFlight"] += 1
        _stats["providerMaxInFlight"] = max(
            _stats["providerMaxInFlight"], _stats["providerInFlight"]
        )


def provider_finished():
    with _stats_lock:
        _stats["providerInFlight"] -= 1


def stats_snapshot():
    # Copy each independently locked structure rather than holding both locks at
    # once. The endpoint is polled while requests are in flight, and avoiding a
    # cross-lock order makes the test double incapable of deadlocking the load it
    # is supposed to observe. The final assertion happens only at active == 0.
    with _stats_lock:
        counters = dict(_stats)
    with _lock:
        reservation_states = {
            state: sum(1 for item in _reservations.values() if item["state"] == state)
            for state in ("ACTIVE", "COMPLETED", "ABANDONED", "CANCELLED", "EXPIRED")
        }
        reservations_total = len(_reservations)
        reserved_usd = _reserved_usd
        settlement_records = len(_settlements)
        admit_bodies = [dict(body) for body in _admit_bodies]
        provider_openai_requests = [dict(item) for item in _provider_openai_requests]
    return {
        "counters": counters,
        "reservations": {
            "total": reservations_total,
            "active": reservation_states["ACTIVE"],
            "completed": reservation_states["COMPLETED"],
            "abandoned": reservation_states["ABANDONED"],
            "cancelled": reservation_states["CANCELLED"],
            "expired": reservation_states["EXPIRED"],
            "reservedUsd": reserved_usd,
        },
        "settlementRecords": settlement_records,
        "admitBodies": admit_bodies,
        "providerOpenaiRequests": provider_openai_requests,
    }


def record_event(event):
    if not EVENTS_FILE:
        return
    with open(EVENTS_FILE, "a") as f:
        f.write(json.dumps(event) + "\n")


def _apply_cost(cost):
    """Add a settled cost to both scopes. Caller holds _lock."""
    for w in ("1d_rolling", "7d_rolling", "30d_rolling", "1mo_calendar"):
        _state["cost"][w] += cost
        _org_state["cost"][w] += cost


def _reap_expired_reservations_locked(now_ms):
    """Release expired ACTIVE holds exactly as the platform lease reaper does.

    Caller holds `_lock`. The dedicated Worker deadline phase asserts this is
    never needed for a request whose upstream is still live: the gateway must
    abandon first, before the lease can expire underneath the provider call.
    """
    global _reserved_usd
    for rid, reservation in _reservations.items():
        if (
            reservation["state"] == "ACTIVE"
            and reservation["expiresAtMonotonicMs"] <= now_ms
        ):
            reservation["state"] = "EXPIRED"
            _reserved_usd -= reservation["costUsd"]
            if abs(_reserved_usd) < 1e-12:
                _reserved_usd = 0.0
            if _by_request.get(reservation["requestId"]) == rid:
                del _by_request[reservation["requestId"]]
            bump_stat("reservationReaped")
            log(
                "REAP reservation=%s requestId=%s after %dms lease"
                % (rid, reservation["requestId"], RESERVATION_LEASE_MS)
            )


class ConcurrentHTTPServer(ThreadingHTTPServer):
    # A 16-client phase fans out into state, admission, provider and settlement
    # requests. The stdlib TCPServer backlog is only five; raising it prevents the
    # test double itself from serializing or refusing the burst under test.
    request_queue_size = 64


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body=None, headers=None):
        if code >= 400:
            bump_stat("httpErrors")
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if body is not None:
            payload = json.dumps(body).encode()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_header("Content-Length", "0")
            self.end_headers()

    def do_GET(self):
        if self.path == "/__mock/stats":
            return self._send(200, stats_snapshot(), {"Cache-Control": "no-store"})
        if self.path.endswith("/policies/effective"):
            fetch_number = bump_stat("policyFetchRequests")
            if POLICY_RACE:
                if fetch_number == 1:
                    log("GET /effective -> 200 OLD after %dms delay" % POLICY_RACE_OLD_DELAY_MS)
                    time.sleep(POLICY_RACE_OLD_DELAY_MS / 1000.0)
                    return self._send(200, POLICY_RACE_OLD, {"ETag": POLICY_RACE_OLD_ETAG})
                log("GET /effective -> 200 NEW immediately")
                return self._send(200, POLICY_RACE_NEW, {"ETag": POLICY_RACE_NEW_ETAG})
            if self.headers.get("If-None-Match") == POLICIES_ETAG:
                log("GET /effective -> 304 (unchanged)")
                return self._send(304, headers={"ETag": POLICIES_ETAG})
            if NO_POLICIES:
                log("GET /effective -> 200 (no policies)")
            else:
                log("GET /effective -> 200 (%d policies, type=%s, maxUsd=%s, failClosed=%s)"
                    % (len(POLICIES["policies"]), POLICY_TYPE, MAX_USD, FAIL_CLOSED))
            return self._send(200, POLICIES, {"ETag": POLICIES_ETAG, "Cache-Control": "private, max-age=30"})
        if self.path.endswith("/policies/state"):
            if STATE_UNAVAILABLE:
                # Both durable stores are gone. 503 — never a 200 full of
                # zeros, which a gateway cannot tell from an idle project.
                log("GET /state -> 503 (GUARDRAIL_STATE_UNAVAILABLE)")
                return self._send(503, {"error": {"code": "GUARDRAIL_STATE_UNAVAILABLE"}})
            with _lock:
                body = dict(_state)
                if SERVE_ORG_STATE:
                    body["org"] = dict(_org_state)
                body["asOf"] = "2026-07-28T00:00:00Z"
                body["ttlSeconds"] = 30
                body["stale"] = False
                spent = _state["cost"][WINDOW]
                org_spent = _org_state["cost"][WINDOW]
            log("GET /state -> 200 (cost[%s]=%.4f, org=%s / cap %.4f)"
                % (WINDOW, spent, ("%.4f" % org_spent) if SERVE_ORG_STATE else "absent", MAX_USD))
            return self._send(200, body)
        return self._send(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        if self.path.endswith("/policies/usage"):
            try:
                data = json.loads(raw or b"[]")
            except Exception:
                return self._send(400, {"success": False, "error": "bad json"})
            events = data if isinstance(data, list) else [data]
            accepted = persisted = blocked = 0
            with _lock:
                for e in events:
                    accepted += 1
                    record_event(e)
                    outcome = e.get("outcome", "ALLOWED")
                    if outcome == "BLOCKED":
                        blocked += 1
                        log("  <- BLOCKED event: blockedBy=%s policyId=%s reason=%r"
                            % (e.get("blockedBy"), e.get("policyId"), e.get("reason")))
                        # blocked events are NOT metered
                        continue
                    persisted += 1
                    cost = float(e.get("costUsd", 0.0) or 0.0)
                    # Apply to BOTH scopes, like the backend's dual-prefix write.
                    for w in ("1d_rolling", "7d_rolling", "30d_rolling", "1mo_calendar"):
                        _state["cost"][w] += cost
                        _org_state["cost"][w] += cost
                    log("  <- ALLOWED event: model=%s costUsd=%.5f (project total %.5f, org total %.5f)"
                        % (e.get("model"), cost, _state["cost"][WINDOW], _org_state["cost"][WINDOW]))
            return self._send(202, {"success": True, "accepted": accepted, "persisted": persisted, "blocked": blocked})
        if self.path.endswith("/policies/admit"):
            return self._handle_admit(raw)
        if "/policies/reservations/" in self.path:
            return self._handle_settlement(raw)
        if self.path.startswith("/v1/chat/completions"):
            return self._with_provider(lambda: self._handle_openai(raw))
        if self.path.startswith("/v1/messages"):
            return self._with_provider(lambda: self._handle_anthropic(raw))
        return self._send(404, {"error": "not found"})

    def _with_provider(self, handle):
        provider_started()
        try:
            if PROVIDER_DELAY_MS:
                time.sleep(PROVIDER_DELAY_MS / 1000.0)
            return handle()
        finally:
            provider_finished()

    def _handle_openai(self, raw):
        # Mock OpenAI provider: echo the model, fixed content, configurable usage.
        try:
            req = json.loads(raw or b"{}")
        except Exception:
            req = {}
        with _lock:
            _provider_openai_requests.append({
                "rawBytes": len(raw),
                "body": req,
            })
        model = req.get("model", "gpt-4o")
        if req.get("stream") is True:
            bump_stat("providerOpenaiStreaming")
            return self._stream_completion(req, model)
        bump_stat("providerOpenaiBuffered")
        log("POST /v1/chat/completions (mock provider) model=%s -> 200" % model)
        return self._send(200, {
            "id": "chatcmpl-mock-e2e",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "mock completion"},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": PROMPT_TOKENS,
                "completion_tokens": COMPLETION_TOKENS,
                "total_tokens": PROMPT_TOKENS + COMPLETION_TOKENS,
            },
        })

    # --- Admission ---------------------------------------------------------

    def _handle_admit(self, raw):
        """POST .../policies/admit — the atomic reservation.

        Contract points that matter and are easy to get wrong:
          * a BLOCK is HTTP 200 with {"allowed": false}, not a 4xx;
          * an allow MUST carry a non-empty reservationId, or the gateway
            cannot settle it and treats the answer as unavailable;
          * the same requestId must REPLAY the existing reservation, because
            the gateway retries a 5xx/429 with the same key.
        """
        global _reservation_seq, _reserved_usd
        bump_stat("admitRequests")
        if ADMIT_UNAVAILABLE:
            log("POST /admit -> 503 (GUARDRAIL_ADMISSION_UNAVAILABLE)")
            return self._send(503, {"message": "GUARDRAIL_ADMISSION_UNAVAILABLE"})
        try:
            req = json.loads(raw or b"{}")
        except Exception:
            return self._send(400, {"error": "bad json"})

        request_id = str(req.get("requestId") or "")
        est = float(req.get("estimatedCostUsd", 0.0) or 0.0)
        with _lock:
            _admit_bodies.append(dict(req))
            now_ms = time.monotonic() * 1000
            _reap_expired_reservations_locked(now_ms)
            existing = _by_request.get(request_id)
            if existing:
                bump_stat("admitReplayed")
                log("POST /admit -> 200 REPLAY reservation=%s requestId=%s" % (existing, request_id))
                return self._send(200, {
                    "allowed": True,
                    "reservationId": existing,
                    "expiresAt": "2026-08-12T00:05:00Z",
                    "policyVersion": POLICIES_ETAG,
                    "replayed": True,
                })

            spent = _state["cost"][WINDOW]
            projected = spent + _reserved_usd + est
            if projected > MAX_USD:
                bump_stat("admitBlocked")
                log("POST /admit -> 200 BLOCKED (spent=%.6f reserved=%.6f est=%.6f projected=%.6f > cap %.6f)"
                    % (spent, _reserved_usd, est, projected, MAX_USD))
                return self._send(200, {
                    "allowed": False,
                    "decision": {
                        "policyId": "pol_mock_cost_cap",
                        "policyName": "E2E daily cost cap",
                        "policyType": "COST_CAP",
                        "scope": POLICY_SOURCE,
                        "dimension": WINDOW,
                        "limit": MAX_USD,
                        "observed": spent + _reserved_usd,
                        "projected": projected,
                        "reason": "projected spend %.6f exceeds the %s cap of %.6f" % (projected, WINDOW, MAX_USD),
                    },
                })

            _reservation_seq += 1
            rid = "res_mock_%d" % _reservation_seq
            _reservations[rid] = {
                "requestId": request_id,
                "costUsd": est,
                "state": "ACTIVE",
                "expiresAtMonotonicMs": now_ms + RESERVATION_LEASE_MS,
            }
            _by_request[request_id] = rid
            _reserved_usd += est
            bump_stat("admitNewAllowed")
            log("POST /admit -> 200 ALLOWED reservation=%s model=%s in=%s maxOut=%s est=$%.6f "
                "pricingVersion=%s (held total $%.6f)"
                % (rid, req.get("model"), req.get("estimatedInputTokens"),
                   req.get("maximumOutputTokens"), est,
                   req.get("pricingVersion", "ABSENT"), _reserved_usd))
            return self._send(200, {
                "allowed": True,
                "reservationId": rid,
                "expiresAt": "2026-08-12T00:05:00Z",
                "policyVersion": POLICIES_ETAG,
                "replayed": False,
            })

    def _handle_settlement(self, raw):
        """POST .../policies/reservations/{rid}/{complete|abandon|cancel}."""
        global _reserved_usd
        parts = self.path.rstrip("/").split("/")
        endpoint = parts[-1]
        rid = parts[-2]
        try:
            body = json.loads(raw or b"{}")
        except Exception:
            body = {}
        if endpoint not in ("complete", "abandon", "cancel"):
            return self._send(404, {"error": "unknown settlement endpoint %r" % endpoint})
        bump_stat("settlementRequests")

        with _lock:
            res = _reservations.get(rid)
            if res is None:
                log("POST /reservations/%s/%s -> 404 (unknown reservation)" % (rid, endpoint))
                return self._send(404, {"error": "unknown reservation"})
            if res["state"] != "ACTIVE":
                # Settlement is idempotent per reservation: the gateway retries.
                bump_stat("settlementIdempotent")
                log("POST /reservations/%s/%s -> 202 (already %s, idempotent)" % (rid, endpoint, res["state"]))
                return self._send(202, {"success": True, "idempotent": True})

            held = res["costUsd"]
            _reserved_usd -= held
            # Estimates are floating-point in this test double. Concurrently
            # adding/subtracting the same 16 holds can leave a ~1e-17 residue;
            # canonicalize that arithmetic noise so "no active hold" is also
            # represented as an exact zero in the observable ledger.
            if abs(_reserved_usd) < 1e-12:
                _reserved_usd = 0.0
            if endpoint == "complete":
                # The real usage REPLACES the estimate. This is the number that
                # proves the stream tee worked: it comes from the terminal SSE
                # usage frame, not from input + max_tokens.
                actual = float(body.get("costUsd", 0.0) or 0.0)
                _apply_cost(actual)
                res["state"] = "COMPLETED"
                bump_stat("settlementCompleted")
                log("POST /reservations/%s/complete -> 202  in=%s out=%s model=%s cost=$%.8f "
                    "pricingVersion=%s (estimate was $%.8f, released; project total $%.8f)"
                    % (rid, body.get("inputTokens"), body.get("outputTokens"),
                       body.get("model"), actual, body.get("pricingVersion", "ABSENT"),
                       held, _state["cost"][WINDOW]))
            elif endpoint == "abandon":
                # The call may have reached the provider and no usage came back,
                # so the conservative estimate STAYS applied.
                _apply_cost(held)
                res["state"] = "ABANDONED"
                bump_stat("settlementAbandoned")
                log("POST /reservations/%s/abandon -> 202  reason=%r (estimate $%.8f RETAINED)"
                    % (rid, body.get("reason"), held))
            else:
                # Provably never reached the provider: release the hold.
                res["state"] = "CANCELLED"
                bump_stat("settlementCancelled")
                log("POST /reservations/%s/cancel -> 202  reason=%r (estimate $%.8f released)"
                    % (rid, body.get("reason"), held))
            _settlements.append({"reservationId": rid, "endpoint": endpoint, "body": body})
            record_event({"settlement": endpoint, "reservationId": rid, **body})
        return self._send(202, {"success": True})

    # --- Mock provider: streaming ------------------------------------------

    def _stream_completion(self, req, model):
        """An OpenAI-shaped SSE stream ending in a usage frame.

        Framing matters: `SseFrameBuffer` splits on a BLANK LINE, so every frame
        must end `\\n\\n`. Each frame is flushed separately (with a small pause)
        so they arrive as distinct transport chunks and the gateway's tee is
        exercised across chunk boundaries rather than on one tidy buffer.
        """
        include_usage = bool(
            (req.get("stream_options") or {}).get("include_usage")
            if isinstance(req.get("stream_options"), dict) else False
        )
        log("POST /v1/chat/completions (mock provider) model=%s stream=true "
            "stream_options.include_usage=%s -> 200 text/event-stream"
            % (model, include_usage))
        if not include_usage:
            log("  !! include_usage was NOT set on the inbound request; the gateway "
                "is supposed to FORCE it while metering")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def frame(obj):
            self.wfile.write(("data: %s\n\n" % json.dumps(obj)).encode())
            self.wfile.flush()
            if STREAM_DELAY_MS:
                time.sleep(STREAM_DELAY_MS / 1000.0)

        base = {"id": "chatcmpl-mock-stream", "object": "chat.completion.chunk", "model": model}
        frame({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}}]})
        frame({**base, "choices": [{"index": 0, "delta": {"content": " world"}}]})
        frame({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        if not STREAM_NO_USAGE:
            frame({**base, "choices": [], "usage": {
                "prompt_tokens": STREAM_PROMPT_TOKENS,
                "completion_tokens": STREAM_COMPLETION_TOKENS,
                "total_tokens": STREAM_PROMPT_TOKENS + STREAM_COMPLETION_TOKENS,
            }})
        else:
            log("  (MOCK_STREAM_NO_USAGE: omitting the terminal usage frame; "
                "the reservation must ABANDON, not complete)")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    # --- Mock provider: Anthropic Messages --------------------------------

    def _handle_anthropic(self, raw):
        try:
            req = json.loads(raw or b"{}")
        except Exception:
            req = {}
        model = req.get("model", "claude-sonnet-5")
        roles = [m.get("role") for m in req.get("messages", []) if isinstance(m, dict)]
        tools = [t.get("name") for t in req.get("tools", []) if isinstance(t, dict)]
        system = req.get("system", "")
        if isinstance(system, list):
            system = "|".join(
                str(block.get("text", "")) for block in system if isinstance(block, dict)
            )
        forbidden_names = {
            "accept-encoding",
            "connection",
            "content-encoding",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "x-hop-by-hop-e2e",
        }
        filtered_header_leaks = sorted({
            name.lower()
            for name in self.headers.keys()
            if name.lower() in forbidden_names
            or name.lower().startswith("x-noveum-")
        })
        api_key = self.headers.get("x-api-key")
        api_key_marker = {
            "sk-ant-test": "bearer-test",
            "sk-ant-direct-test": "direct-test",
        }.get(api_key, "present-other" if api_key else "absent")
        log(
            "POST %s (mock anthropic) model=%s stream=%s max_tokens=%r "
            "max_completion_present=%s stream_options_present=%s roles=%s tools=%s "
            "system=%r x_api_key=%s anthropic_beta=%r filtered_header_leaks=%r "
            "x_api_key_marker=%s"
            % (
                self.path,
                model,
                req.get("stream") is True,
                req.get("max_tokens"),
                "max_completion_tokens" in req,
                "stream_options" in req,
                roles,
                tools,
                system,
                bool(api_key),
                self.headers.get("anthropic-beta"),
                filtered_header_leaks,
                api_key_marker,
            )
        )

        if req.get("stream") is True:
            bump_stat("providerAnthropicStreaming")
            return self._stream_anthropic(req, model, tools)

        bump_stat("providerAnthropicBuffered")
        content = []
        stop_reason = "end_turn"
        if tools:
            content.append({
                "type": "tool_use",
                "id": "toolu_weather_1",
                "name": tools[0],
                "input": {"city": "Paris"},
            })
            stop_reason = "tool_use"
        else:
            content.append({"type": "text", "text": "mock Claude completion"})
        return self._send(200, {
            "id": "msg_mock_buffered",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": content,
            "stop_reason": stop_reason,
            "usage": {
                "input_tokens": STREAM_PROMPT_TOKENS,
                "output_tokens": STREAM_COMPLETION_TOKENS,
            },
        }, {"request-id": "req_mock_anthropic"})

    def _stream_anthropic(self, req, model, tools):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("request-id", "req_mock_anthropic_stream")
        self.end_headers()

        def event(kind, payload):
            raw_event = "event: %s\ndata: %s\n\n" % (kind, json.dumps(payload))
            # Deliberately split each SSE frame into awkward tiny writes. TCP is
            # free to coalesce them, but this still exercises arbitrary transport
            # boundaries in workerd instead of one pre-buffered response.
            encoded = raw_event.encode()
            for start in range(0, len(encoded), 3):
                self.wfile.write(encoded[start:start + 3])
                self.wfile.flush()

        event("message_start", {
            "type": "message_start",
            "message": {
                "id": "msg_mock_stream",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "usage": {"input_tokens": STREAM_PROMPT_TOKENS, "output_tokens": 0},
            },
        })
        if tools:
            event("content_block_start", {
                "type": "content_block_start",
                "index": 4,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_weather_1",
                    "name": tools[0],
                    "input": {},
                },
            })
            event("content_block_delta", {
                "type": "content_block_delta",
                "index": 4,
                "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"},
            })
            event("content_block_delta", {
                "type": "content_block_delta",
                "index": 4,
                "delta": {"type": "input_json_delta", "partial_json": "\"Paris\"}"},
            })
            event("content_block_stop", {"type": "content_block_stop", "index": 4})
            stop_reason = "tool_use"
        else:
            event("content_block_start", {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            })
            event("content_block_delta", {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "mock Claude stream"},
            })
            event("content_block_stop", {"type": "content_block_stop", "index": 0})
            stop_reason = "end_turn"
        event("message_delta", {
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": None},
            "usage": {"output_tokens": STREAM_COMPLETION_TOKENS},
        })
        event("message_stop", {"type": "message_stop"})

    def log_message(self, *a):
        pass  # quiet the default per-request stderr spam; we log our own lines


if __name__ == "__main__":
    srv = ConcurrentHTTPServer(("127.0.0.1", PORT), Handler)
    log("listening on http://127.0.0.1:%d  (maxUsd=%s window=%s seedUsd=%s failClosed=%s eventsFile=%r)"
        % (PORT, MAX_USD, WINDOW, _seed, FAIL_CLOSED, EVENTS_FILE))
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
