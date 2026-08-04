#!/usr/bin/env bash
#
# End-to-end smoke test of the platform-managed Nova Guard loop against the REAL
# gateway binary. By default it runs fully self-contained against a bundled mock
# platform + mock OpenAI provider (scripts/novaguard_mock_platform.py) — no real
# backend or provider key required — and exercises BOTH paths:
#
# Phase 1 (BLOCK, spend pre-seeded over the cap):
#   gateway boot -> GET /policies/effective (fetch cost_cap)
#                -> GET /policies/state      (spend already over cap)
#                -> chat request BLOCKED     (no provider call)
#                -> POST /policies/usage      (BLOCKED event, asserted)
#
# Phase 2 (ALLOWED loop, spend starts at zero):
#   chat request ALLOWED -> mock provider 200 (with usage)
#                        -> POST /policies/usage (ALLOWED event with real cost)
#                        -> /state climbs over the cap
#   second chat request  -> BLOCKED (asserted, incl. the BLOCKED usage event)
#
# Every assertion is machine-checked from the mock's recorded usage events
# (outcome/blockedBy/policyId/costUsd) — the script fails, not a human reader,
# when the loop is broken.
#
# Real-backend mode: export NOVEUM_API_URL / NOVEUM_API_KEY / NOVEUM_GUARD_PROJECT_ID
# pointing at a running noveum-app-nextjs (with a low COST_CAP policy on that
# project) and pass --live; the script runs Phase 1 only against the real API.
#
# Usage:
#   scripts/novaguard_e2e.sh            # mock, both phases (default)
#   scripts/novaguard_e2e.sh --live     # real backend from env, block path only
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
GW_PORT="${GW_PORT:-8899}"
GW_PORT2="${GW_PORT2:-8898}"
MOCK_PORT="${MOCK_PORT:-8787}"
MOCK_PORT2="${MOCK_PORT2:-8788}"
LIVE=0
[[ "${1:-}" == "--live" ]] && LIVE=1

EVENTS_DIR="$(mktemp -d)"
pids=()
cleanup() {
  for pid in "${pids[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$EVENTS_DIR"
}
trap cleanup EXIT

fail=0
say_pass() { echo "  ✓ $1"; }
say_fail() { echo "  ✗ $1"; fail=1; }

# assert_event <events.jsonl> <outcome> [<blockedBy> <policyId>]
# Waits up to 8s for a matching usage event; checks exact fields.
assert_event() {
  local file="$1" outcome="$2" blocked_by="${3:-}" policy_id="${4:-}"
  for _ in $(seq 1 40); do
    if [[ -s "$file" ]] && python3 - "$file" "$outcome" "$blocked_by" "$policy_id" <<'PY'
import json, sys
path, outcome, blocked_by, policy_id = sys.argv[1:5]
ok = False
for line in open(path):
    e = json.loads(line)
    if e.get("outcome", "ALLOWED") != outcome:
        continue
    if outcome == "BLOCKED":
        if blocked_by and e.get("blockedBy") != blocked_by:
            continue
        if policy_id and e.get("policyId") != policy_id:
            continue
    else:
        # An ALLOWED event must carry a real (non-zero) cost, else the
        # platform counters never advance and caps can never trip.
        if float(e.get("costUsd", 0) or 0) <= 0:
            continue
    ok = True
    break
sys.exit(0 if ok else 1)
PY
    then return 0; fi
    sleep 0.2
  done
  return 1
}

wait_health() {
  local port="$1"
  for i in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  echo "gateway on :$port did not become healthy"; exit 1
}

chat() { # chat <gw_port> <headers_out>
  curl -sS -D "$2" \
    -X POST "http://127.0.0.1:$1/v1/chat/completions" \
    -H "content-type: application/json" \
    -H "x-provider: openai" \
    -H "authorization: Bearer sk-mock-not-verified" \
    -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hello"}]}' || true
}

echo "== building gateway (debug) =="
cargo build --bin noveum-ai-gateway >/dev/null

# ---------------------------------------------------------------- Phase 1: BLOCK
if [[ "$LIVE" == "1" ]]; then
  : "${NOVEUM_API_URL:?set NOVEUM_API_URL for --live}"
  : "${NOVEUM_API_KEY:?set NOVEUM_API_KEY for --live}"
  : "${NOVEUM_GUARD_PROJECT_ID:?set NOVEUM_GUARD_PROJECT_ID for --live}"
  echo "== LIVE mode against $NOVEUM_API_URL (project $NOVEUM_GUARD_PROJECT_ID) =="
else
  echo "== phase 1: starting mock platform on :$MOCK_PORT (seeded over cap) =="
  BLOCK_EVENTS="$EVENTS_DIR/phase1.jsonl"
  MOCK_PORT="$MOCK_PORT" MOCK_MAX_USD="0.01" MOCK_SEED_USD="5.00" MOCK_FAIL_CLOSED="true" \
    MOCK_EVENTS_FILE="$BLOCK_EVENTS" \
    python3 scripts/novaguard_mock_platform.py &
  pids+=($!)
  sleep 1
  export NOVEUM_API_URL="http://127.0.0.1:$MOCK_PORT"
  export NOVEUM_API_KEY="test-key"
  export NOVEUM_GUARD_PROJECT_ID="e2e-project"
fi

echo "== phase 1: starting gateway on :$GW_PORT =="
PORT="$GW_PORT" HOST="127.0.0.1" \
  NOVEUM_GUARD_BLOCK_RESPONSE_MODE="provider_error" \
  NOVEUM_GUARD_USAGE_FLUSH_MS="200" \
  RUST_LOG="info,noveum_ai_gateway=info" \
  "$ROOT/target/debug/noveum-ai-gateway" &
pids+=($!)
wait_health "$GW_PORT"

echo
echo "== phase 1: sending a chat completion (expected: BLOCKED by cost cap) =="
resp_headers="$(mktemp)"
body="$(chat "$GW_PORT" "$resp_headers")"
echo "--- response body ---"; echo "$body"; echo

echo "== phase 1 assertions =="
if grep -qi "x-noveum-guard-blocked" "$resp_headers"; then
  say_pass "response carries x-noveum-guard-blocked"
else
  say_fail "missing x-noveum-guard-blocked header"
fi
if echo "$body" | grep -qi "noveum_guard_blocked\|Nova Guard\|guard_blocked"; then
  say_pass "body is a Nova Guard block envelope"
else
  say_fail "body is not a guard block envelope"
fi
rm -f "$resp_headers"

if [[ "$LIVE" == "1" ]]; then
  echo "  (live mode: check the platform's usage log for the BLOCKED event)"
else
  if assert_event "$BLOCK_EVENTS" "BLOCKED" "COST_CAP" "pol_mock_cost_cap"; then
    say_pass "BLOCKED usage event received (blockedBy=COST_CAP policyId=pol_mock_cost_cap)"
  else
    say_fail "no matching BLOCKED usage event was posted to /policies/usage"
  fi
fi

# --------------------------------------------------------- Phase 2: ALLOWED loop
if [[ "$LIVE" == "0" ]]; then
  echo
  echo "== phase 2: starting mock platform+provider on :$MOCK_PORT2 (spend 0, cap \$1) =="
  LOOP_EVENTS="$EVENTS_DIR/phase2.jsonl"
  # One mock call reports 100k+100k gpt-4o tokens = $1.25 > the $1 cap, so the
  # very next request (after the state cache expires) must block.
  MOCK_PORT="$MOCK_PORT2" MOCK_MAX_USD="1.00" MOCK_SEED_USD="0.00" MOCK_FAIL_CLOSED="true" \
    MOCK_EVENTS_FILE="$LOOP_EVENTS" \
    python3 scripts/novaguard_mock_platform.py &
  pids+=($!)
  sleep 1

  echo "== phase 2: starting gateway on :$GW_PORT2 (provider = mock) =="
  PORT="$GW_PORT2" HOST="127.0.0.1" \
    NOVEUM_API_URL="http://127.0.0.1:$MOCK_PORT2" \
    NOVEUM_API_KEY="test-key" \
    NOVEUM_GUARD_PROJECT_ID="e2e-project" \
    OPENAI_BASE_URL="http://127.0.0.1:$MOCK_PORT2" \
    NOVEUM_GUARD_BLOCK_RESPONSE_MODE="provider_error" \
    NOVEUM_GUARD_USAGE_FLUSH_MS="200" \
    RUST_LOG="info,noveum_ai_gateway=info" \
    "$ROOT/target/debug/noveum-ai-gateway" &
  pids+=($!)
  wait_health "$GW_PORT2"

  echo "== phase 2: first request (expected: ALLOWED, reaches mock provider) =="
  h2="$(mktemp)"
  body2="$(chat "$GW_PORT2" "$h2")"
  if grep -qi "x-noveum-guard-blocked" "$h2"; then
    say_fail "first request was blocked; expected ALLOWED"
  elif echo "$body2" | grep -q "mock completion"; then
    say_pass "first request reached the mock provider"
  else
    say_fail "first request did not return the mock completion: $body2"
  fi

  if assert_event "$LOOP_EVENTS" "ALLOWED"; then
    say_pass "ALLOWED usage event with non-zero costUsd was reported"
  else
    say_fail "no ALLOWED usage event with non-zero costUsd was reported"
  fi

  echo "== phase 2: waiting out the gateway's /state cache TTL =="
  sleep 11

  echo "== phase 2: second request (expected: BLOCKED, spend now over cap) =="
  h3="$(mktemp)"
  chat "$GW_PORT2" "$h3" >/dev/null
  if grep -qi "x-noveum-guard-blocked" "$h3"; then
    say_pass "second request blocked after usage advanced the counters"
  else
    say_fail "second request was NOT blocked; the ALLOWED->usage->state->block loop is broken"
  fi
  if assert_event "$LOOP_EVENTS" "BLOCKED" "COST_CAP" "pol_mock_cost_cap"; then
    say_pass "BLOCKED usage event for the loop was reported"
  else
    say_fail "no BLOCKED usage event for the loop was reported"
  fi
  rm -f "$h2" "$h3"
fi

echo
if [[ "$fail" == "0" ]]; then
  echo "NOVA GUARD E2E: PASS"
else
  echo "NOVA GUARD E2E: FAIL"; exit 1
fi
