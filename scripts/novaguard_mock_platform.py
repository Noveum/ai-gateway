#!/usr/bin/env python3
"""A tiny stand-in for the Noveum platform NovaGuard API, for local end-to-end
testing of the ai-gateway platform bridge WITHOUT the real backend.

Implements the three project-scoped endpoints the gateway calls:

  GET  /api/v1/projects/{id}/policies/effective   -> the policy set (+ ETag/304)
  GET  /api/v1/projects/{id}/policies/state        -> live cost/rate counters
  POST /api/v1/projects/{id}/policies/usage        -> 202; ALLOWED events add to
                                                      the cost counter (so caps
                                                      climb and eventually trip)

plus a mock OpenAI-compatible provider (so the ALLOWED path can be exercised
hermetically, with the gateway's OPENAI_BASE_URL pointed here):

  POST /v1/chat/completions                        -> 200 chat completion with a
                                                      configurable usage block

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

Every request is logged to stderr; received usage events are printed (and, when
MOCK_EVENTS_FILE is set, appended as JSON lines so a test harness can assert on
them). Bearer auth is accepted but not verified (any token passes) — this is a
test double, not the real auth plane.
"""
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
            "config": {"window": WINDOW, "maxUsd": MAX_USD, "action": "BLOCK"},
        }
    ]
}
POLICIES_ETAG = '"mock-policies-v1"'


def log(*a):
    print("[mock-platform]", *a, file=sys.stderr, flush=True)


def record_event(event):
    if not EVENTS_FILE:
        return
    with open(EVENTS_FILE, "a") as f:
        f.write(json.dumps(event) + "\n")


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
        if self.path.startswith("/v1/chat/completions"):
            # Mock OpenAI provider: echo the model, fixed content, configurable usage.
            try:
                req = json.loads(raw or b"{}")
            except Exception:
                req = {}
            model = req.get("model", "gpt-4o")
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
