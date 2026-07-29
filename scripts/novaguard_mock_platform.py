#!/usr/bin/env python3
"""A tiny stand-in for the Noveum platform NovaGuard API, for local end-to-end
testing of the ai-gateway platform bridge WITHOUT the real backend.

Implements the three project-scoped endpoints the gateway calls:

  GET  /api/v1/projects/{id}/policies/effective   -> the policy set (+ ETag/304)
  GET  /api/v1/projects/{id}/policies/state        -> live cost/rate counters
  POST /api/v1/projects/{id}/policies/usage        -> 202; ALLOWED events add to
                                                      the cost counter (so caps
                                                      climb and eventually trip)

Config via env:
  MOCK_PORT        (default 8787)
  MOCK_MAX_USD     cost_cap maxUsd            (default 0.01)
  MOCK_WINDOW      cost_cap window            (default 1d_rolling)
  MOCK_SEED_USD    initial cost in the window (default 0.0)
  MOCK_FAIL_CLOSED policy failClosed          (default true)

Every request is logged to stderr; received usage events are printed so you can
watch ALLOWED/BLOCKED events arrive. Bearer auth is accepted but not verified
(any token passes) — this is a test double, not the real auth plane.
"""
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("MOCK_PORT", "8787"))
MAX_USD = float(os.environ.get("MOCK_MAX_USD", "0.01"))
WINDOW = os.environ.get("MOCK_WINDOW", "1d_rolling")
FAIL_CLOSED = os.environ.get("MOCK_FAIL_CLOSED", "true").lower() in ("1", "true", "yes")

_lock = threading.Lock()
# Live, mutable state. ALLOWED usage events accumulate into `cost` so a cost cap
# climbs and trips exactly like the real durable counters.
_state = {
    "cost": {"1d_rolling": 0.0, "7d_rolling": 0.0, "30d_rolling": 0.0, "1mo_calendar": 0.0, "perModel": {}},
    "rate": {"requests_1m": 0, "requests_1h": 0, "requests_1d": 0, "tokens_1m": 0, "tokens_1h": 0, "tokens_1d": 0},
}
_seed = float(os.environ.get("MOCK_SEED_USD", "0.0"))
for w in ("1d_rolling", "7d_rolling", "30d_rolling", "1mo_calendar"):
    _state["cost"][w] = _seed

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
            "source": "project",
            "config": {"window": WINDOW, "maxUsd": MAX_USD, "action": "BLOCK"},
        }
    ]
}
POLICIES_ETAG = '"mock-policies-v1"'


def log(*a):
    print("[mock-platform]", *a, file=sys.stderr, flush=True)


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
            with _lock:
                body = dict(_state)
                body["asOf"] = "2026-07-28T00:00:00Z"
                body["ttlSeconds"] = 30
                body["stale"] = False
                spent = _state["cost"][WINDOW]
            log("GET /state -> 200 (cost[%s]=%.4f / cap %.4f)" % (WINDOW, spent, MAX_USD))
            return self._send(200, body)
        return self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path.endswith("/policies/usage"):
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length) if length else b"[]"
            try:
                data = json.loads(raw)
            except Exception:
                return self._send(400, {"success": False, "error": "bad json"})
            events = data if isinstance(data, list) else [data]
            accepted = persisted = blocked = 0
            with _lock:
                for e in events:
                    accepted += 1
                    outcome = e.get("outcome", "ALLOWED")
                    if outcome == "BLOCKED":
                        blocked += 1
                        log("  <- BLOCKED event: blockedBy=%s policyId=%s reason=%r"
                            % (e.get("blockedBy"), e.get("policyId"), e.get("reason")))
                        # blocked events are NOT metered
                        continue
                    persisted += 1
                    cost = float(e.get("costUsd", 0.0) or 0.0)
                    for w in ("1d_rolling", "7d_rolling", "30d_rolling", "1mo_calendar"):
                        _state["cost"][w] += cost
                    log("  <- ALLOWED event: model=%s costUsd=%.5f (window total now %.5f)"
                        % (e.get("model"), cost, _state["cost"][WINDOW]))
            return self._send(202, {"success": True, "accepted": accepted, "persisted": persisted, "blocked": blocked})
        return self._send(404, {"error": "not found"})

    def log_message(self, *a):
        pass  # quiet the default per-request stderr spam; we log our own lines


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    log("listening on http://127.0.0.1:%d  (maxUsd=%s window=%s seedUsd=%s failClosed=%s)"
        % (PORT, MAX_USD, WINDOW, _seed, FAIL_CLOSED))
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
