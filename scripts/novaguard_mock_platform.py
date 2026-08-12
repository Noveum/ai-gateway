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

plus a mock OpenAI-compatible provider (so the ALLOWED path can be exercised
hermetically, with the gateway's OPENAI_BASE_URL pointed here):

  POST /v1/chat/completions                        -> 200 chat completion with a
                                                      configurable usage block,
                                                      or, for `"stream": true`,
                                                      an OpenAI-shaped SSE stream
                                                      ending in a usage frame

Config via env:
  MOCK_PORT              (default 8787)
  MOCK_MAX_USD           cost_cap maxUsd            (default 0.01)
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
# cost_cap `enforcementMode`. STRICT selects platform-atomic admission, which is
# also what makes an unavailable /admit fail CLOSED; unset/ADVISORY leaves the
# cap on the in-process ledger, where a 503 from /admit is logged and the request
# proceeds (the cap is still evaluated against live /state).
ENFORCEMENT_MODE = os.environ.get("MOCK_ENFORCEMENT_MODE", "").strip().upper()

_lock = threading.Lock()

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

POLICIES = {
    "policies": [
        {
            "policyId": "pol_mock_cost_cap",
            "name": "E2E daily cost cap",
            "type": "COST_CAP",
            "enabled": True,
            "failClosed": FAIL_CLOSED,
            "mode": None,  # deprecated/null on the real backend; gateway should still ENFORCE
            "priority": 10,
            "source": POLICY_SOURCE,
            "config": dict(
                {"window": WINDOW, "maxUsd": MAX_USD, "action": "BLOCK"},
                **({"enforcementMode": ENFORCEMENT_MODE} if ENFORCEMENT_MODE else {}),
            ),
        }
    ]
}
# Derived from the policy CONTENT, not a constant. A fixed ETag makes the mock
# answer 304 after a knob changes the policy set, so a gateway that already
# fetched once keeps enforcing the previous configuration and the test silently
# measures the wrong thing.
POLICIES_ETAG = '"mock-%s"' % hashlib.sha1(
    json.dumps(POLICIES, sort_keys=True).encode()
).hexdigest()[:16]

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


def log(*a):
    print("[mock-platform]", *a, file=sys.stderr, flush=True)


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


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body=None, headers=None):
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
        if self.path.endswith("/policies/effective"):
            if self.headers.get("If-None-Match") == POLICIES_ETAG:
                log("GET /effective -> 304 (unchanged)")
                return self._send(304, headers={"ETag": POLICIES_ETAG})
            log("GET /effective -> 200 (1 cost_cap, maxUsd=%s, failClosed=%s)" % (MAX_USD, FAIL_CLOSED))
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
            # Mock OpenAI provider: echo the model, fixed content, configurable usage.
            try:
                req = json.loads(raw or b"{}")
            except Exception:
                req = {}
            model = req.get("model", "gpt-4o")
            if req.get("stream") is True:
                return self._stream_completion(req, model)
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
        return self._send(404, {"error": "not found"})

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
            existing = _by_request.get(request_id)
            if existing:
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
            _reservations[rid] = {"requestId": request_id, "costUsd": est, "state": "ACTIVE"}
            _by_request[request_id] = rid
            _reserved_usd += est
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

        with _lock:
            res = _reservations.get(rid)
            if res is None:
                log("POST /reservations/%s/%s -> 404 (unknown reservation)" % (rid, endpoint))
                return self._send(404, {"error": "unknown reservation"})
            if res["state"] != "ACTIVE":
                # Settlement is idempotent per reservation: the gateway retries.
                log("POST /reservations/%s/%s -> 202 (already %s, idempotent)" % (rid, endpoint, res["state"]))
                return self._send(202, {"success": True, "idempotent": True})

            held = res["costUsd"]
            _reserved_usd -= held
            if endpoint == "complete":
                # The real usage REPLACES the estimate. This is the number that
                # proves the stream tee worked: it comes from the terminal SSE
                # usage frame, not from input + max_tokens.
                actual = float(body.get("costUsd", 0.0) or 0.0)
                _apply_cost(actual)
                res["state"] = "COMPLETED"
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
                log("POST /reservations/%s/abandon -> 202  reason=%r (estimate $%.8f RETAINED)"
                    % (rid, body.get("reason"), held))
            else:
                # Provably never reached the provider: release the hold.
                res["state"] = "CANCELLED"
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

    def log_message(self, *a):
        pass  # quiet the default per-request stderr spam; we log our own lines


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    log("listening on http://127.0.0.1:%d  (maxUsd=%s window=%s seedUsd=%s failClosed=%s eventsFile=%r)"
        % (PORT, MAX_USD, WINDOW, _seed, FAIL_CLOSED, EVENTS_FILE))
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
