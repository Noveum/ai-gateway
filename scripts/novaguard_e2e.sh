#!/usr/bin/env bash
#
# End-to-end smoke test of the platform-managed Nova Guard loop against the REAL
# gateway binary. By default it runs fully self-contained against a bundled mock
# platform (scripts/novaguard_mock_platform.py) — no real backend or provider key
# required — and exercises the BLOCKED path, which short-circuits before any
# upstream provider call:
#
#   gateway boot -> GET /policies/effective (fetch cost_cap)
#                -> GET /policies/state      (spend already over cap)
#                -> chat request BLOCKED     (no provider call)
#                -> POST /policies/usage      (BLOCKED event)
#
# Real-backend mode: export NOVEUM_API_URL / NOVEUM_API_KEY / NOVEUM_GUARD_PROJECT_ID
# pointing at a running noveum-app-nextjs (with a low COST_CAP policy on that
# project) and pass --live; the script skips the mock and drives the real API.
#
# The ALLOWED path (a successful call reporting cost+tokens) needs a real upstream
# provider, so it's covered hermetically by the wiremock integration test
# `allowed_exporter_reports_successful_call` rather than this binary smoke test.
#
# Usage:
#   scripts/novaguard_e2e.sh            # mock, block path (default)
#   scripts/novaguard_e2e.sh --live     # real backend from env
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
GW_PORT="${GW_PORT:-8899}"
MOCK_PORT="${MOCK_PORT:-8787}"
LIVE=0
[[ "${1:-}" == "--live" ]] && LIVE=1

pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT

echo "== building gateway (debug) =="
cargo build --bin noveum-ai-gateway >/dev/null

if [[ "$LIVE" == "1" ]]; then
  : "${NOVEUM_API_URL:?set NOVEUM_API_URL for --live}"
  : "${NOVEUM_API_KEY:?set NOVEUM_API_KEY for --live}"
  : "${NOVEUM_GUARD_PROJECT_ID:?set NOVEUM_GUARD_PROJECT_ID for --live}"
  echo "== LIVE mode against $NOVEUM_API_URL (project $NOVEUM_GUARD_PROJECT_ID) =="
else
  echo "== starting mock platform on :$MOCK_PORT (seeded over cap) =="
  # Seed spend well over a tiny cap so the very first request blocks.
  MOCK_PORT="$MOCK_PORT" MOCK_MAX_USD="0.01" MOCK_SEED_USD="5.00" MOCK_FAIL_CLOSED="true" \
    python3 scripts/novaguard_mock_platform.py &
  pids+=($!)
  sleep 1
  export NOVEUM_API_URL="http://127.0.0.1:$MOCK_PORT"
  export NOVEUM_API_KEY="test-key"
  export NOVEUM_GUARD_PROJECT_ID="e2e-project"
fi

echo "== starting gateway on :$GW_PORT =="
PORT="$GW_PORT" HOST="127.0.0.1" \
  NOVEUM_GUARD_BLOCK_RESPONSE_MODE="provider_error" \
  RUST_LOG="info,noveum_ai_gateway=info" \
  "$ROOT/target/debug/noveum-ai-gateway" &
pids+=($!)

echo "== waiting for gateway health =="
for i in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:$GW_PORT/health" >/dev/null 2>&1; then break; fi
  sleep 0.5
  [[ "$i" == "30" ]] && { echo "gateway did not become healthy"; exit 1; }
done

echo
echo "== sending a chat completion (expected: BLOCKED by cost cap) =="
resp_headers="$(mktemp)"
body="$(curl -sS -D "$resp_headers" \
  -X POST "http://127.0.0.1:$GW_PORT/v1/chat/completions" \
  -H "content-type: application/json" \
  -H "x-provider: openai" \
  -H "authorization: Bearer sk-not-used-request-is-blocked-first" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hello"}]}' || true)"

echo "--- response headers ---"; cat "$resp_headers"
echo "--- response body ---"; echo "$body"; echo

echo "== assertions =="
fail=0
if grep -qi "x-noveum-guard-blocked" "$resp_headers"; then
  echo "  ✓ response carries x-noveum-guard-blocked"
else
  echo "  ✗ missing x-noveum-guard-blocked header"; fail=1
fi
if echo "$body" | grep -qi "noveum_guard_blocked\|Nova Guard\|guard_blocked"; then
  echo "  ✓ body is a Nova Guard block envelope"
else
  echo "  ✗ body is not a guard block envelope"; fail=1
fi
rm -f "$resp_headers"

# Give the best-effort usage reporter a moment to flush its BLOCKED event.
sleep 2
echo
echo "== check the mock platform log above for a '<- BLOCKED event' line =="
echo "   (blockedBy=COST_CAP policyId=pol_mock_cost_cap)"

if [[ "$fail" == "0" ]]; then
  echo; echo "E2E BLOCK PATH: PASS"
else
  echo; echo "E2E BLOCK PATH: FAIL"; exit 1
fi
