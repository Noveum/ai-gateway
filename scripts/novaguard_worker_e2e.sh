#!/usr/bin/env bash
# End-to-end proof of the Cloudflare Worker platform bridge against a REAL
# workerd, via `wrangler dev`.
#
# Why this exists: `worker_remote.rs` has thorough unit coverage of every
# decision it makes, but those tests run on the native target. The three things
# that only exist on wasm32 -- `worker::Fetch` to the control plane,
# `ctx.wait_until` settlement, and the SSE stream tee -- were proven only by
# compiling. This script exercises all three inside workerd.
#
# Hermetic: no Cloudflare account, no Noveum backend, no provider key. The
# bundled mock (`novaguard_mock_platform.py`) plays both the control plane and
# the OpenAI upstream; the Worker is pointed at it with NOVEUM_API_URL and
# OPENAI_BASE_URL.
#
# Usage:  scripts/novaguard_worker_e2e.sh
# Env:    WRANGLER_VERSION (default 4.120.0), GATEWAY_PORT (8787),
#         MOCK_PORT (8899), NODE_BIN_DIR (a dir to prepend to PATH for node>=22)
set -uo pipefail

WRANGLER_VERSION="${WRANGLER_VERSION:-4.120.0}"
GATEWAY_PORT="${GATEWAY_PORT:-8787}"
MOCK_PORT="${MOCK_PORT:-8899}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
[ -n "${NODE_BIN_DIR:-}" ] && PATH="$NODE_BIN_DIR:$PATH"
export PATH="$HOME/.cargo/bin:$PATH"

FAILURES=0
MOCK_PID=""
WRANGLER_PID=""

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILURES=$((FAILURES + 1)); }

cleanup() {
  [ -n "$WRANGLER_PID" ] && kill "$WRANGLER_PID" 2>/dev/null
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

# Assert a mock-side log line exists (the mock logs every request it serves, so
# this is how we observe what the Worker actually did, including the calls that
# happen AFTER the client response is complete).
expect_log()   { grep -qF -- "$2" "$1" && pass "$3" || { fail "$3"; echo "      --- mock log ---"; sed 's/^/      /' "$1"; }; }
refute_log()   { grep -qF -- "$2" "$1" && { fail "$3"; sed 's/^/      /' "$1"; } || pass "$3"; }

start_mock() {
  local log="$1"; shift
  pkill -f novaguard_mock_platform.py 2>/dev/null
  sleep 0.5
  env MOCK_PORT="$MOCK_PORT" "$@" python3 "$ROOT/scripts/novaguard_mock_platform.py" >"$log" 2>&1 &
  MOCK_PID=$!
  disown "$MOCK_PID" 2>/dev/null   # else bash announces the pkill as a job death
  for _ in $(seq 1 40); do
    curl -sf -o /dev/null "http://127.0.0.1:$MOCK_PORT/api/v1/projects/p/policies/effective" && return 0
    sleep 0.25
  done
  echo "mock platform did not come up; see $log" >&2
  exit 1
}

start_wrangler() {
  [ -n "$WRANGLER_PID" ] && { kill "$WRANGLER_PID" 2>/dev/null; wait "$WRANGLER_PID" 2>/dev/null; }
  # A fresh process means a fresh isolate, which is the only way to drop the
  # 60s in-isolate policy cache between phases. Restarting is far cheaper than
  # sleeping it out.
  WRANGLER_SEND_METRICS=false npx --yes "wrangler@${WRANGLER_VERSION}" dev \
    --port "$GATEWAY_PORT" --ip 127.0.0.1 --log-level info \
    --var NOVEUM_API_KEY:mock-key \
    --var NOVEUM_GUARD_PROJECT_ID:proj_mock \
    --var "NOVEUM_API_URL:http://127.0.0.1:$MOCK_PORT" \
    --var "OPENAI_BASE_URL:http://127.0.0.1:$MOCK_PORT" \
    >"$TMP/wrangler.log" 2>&1 &
  WRANGLER_PID=$!
  for _ in $(seq 1 240); do
    grep -q "Ready on" "$TMP/wrangler.log" && return 0
    sleep 0.5
  done
  echo "wrangler dev did not come up; see $TMP/wrangler.log" >&2
  cat "$TMP/wrangler.log" >&2
  exit 1
}

stream_request() {
  curl -sN --max-time 30 "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","stream":true,"max_tokens":4096,
         "messages":[{"role":"user","content":"say hi"}]}'
}

command -v node >/dev/null || { echo "node not on PATH (set NODE_BIN_DIR)" >&2; exit 1; }
case "$(node --version)" in v1[0-9].*|v2[01].*) echo "wrangler $WRANGLER_VERSION needs node >= 22, have $(node --version). Set NODE_BIN_DIR." >&2; exit 1;; esac

# ---------------------------------------------------------------------------
say "Phase 1 -- allowed: admit, tee a real stream, settle at the TRUE usage"
# A cap far above the estimate, so admission allows and the interesting part is
# what the reservation settles at.
start_mock "$TMP/p1.log" MOCK_MAX_USD=1.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_STREAM_PROMPT_TOKENS=11 MOCK_STREAM_COMPLETION_TOKENS=4
start_wrangler
BODY="$(stream_request)"
sleep 3   # settlement rides ctx.wait_until, so it lands after the body is done

grep -q 'data: \[DONE\]' <<<"$BODY" && pass "the SSE stream reached the client intact" \
  || { fail "the SSE stream did not reach the client"; echo "$BODY"; }
expect_log "$TMP/p1.log" "GET /effective -> 200" "worker::Fetch reached GET /policies/effective"
expect_log "$TMP/p1.log" "GET /state -> 200"     "worker::Fetch reached GET /policies/state"
expect_log "$TMP/p1.log" "POST /admit -> 200 ALLOWED" "POST /policies/admit reserved"
# The gateway must FORCE include_usage: without it an OpenAI stream carries no
# token counts and every streamed request settles at input + max_tokens.
expect_log "$TMP/p1.log" "stream_options.include_usage=True" \
  "the Worker forced stream_options.include_usage on the upstream request"
# The payoff: settlement carries the REAL counts recovered by the tee, not the
# 2 + 4096 estimate, and it arrives after the response body completed --
# i.e. ctx.wait_until ran.
expect_log "$TMP/p1.log" "/complete -> 202  in=11 out=4" \
  "ctx.wait_until settled the reservation at the stream's TRUE usage (11 in / 4 out)"
refute_log "$TMP/p1.log" "abandon" "nothing was abandoned on the happy path"

# ---------------------------------------------------------------------------
say "Phase 2 -- a stream with no usage frame must ABANDON, never complete at 0"
# Completing at zero would release the whole hold for a call that did run.
start_mock "$TMP/p2.log" MOCK_MAX_USD=1.0 MOCK_ENFORCEMENT_MODE=STRICT MOCK_STREAM_NO_USAGE=1
start_wrangler
stream_request >/dev/null
sleep 3
expect_log "$TMP/p2.log" "/abandon -> 202" "the reservation abandoned"
expect_log "$TMP/p2.log" "response ended without authoritative usage" \
  "abandon carried the reason that distinguishes it in the audit trail"
expect_log "$TMP/p2.log" "RETAINED" "the conservative estimate stayed applied"
refute_log "$TMP/p2.log" "/complete" "it did NOT complete at a fabricated zero"

# ---------------------------------------------------------------------------
say "Phase 3 -- admission 503 against a fail-closed strict cap must BLOCK"
# The sharpest contract point: 503 is 'unevaluable', never an implicit allow.
start_mock "$TMP/p3.log" MOCK_MAX_USD=1.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_FAIL_CLOSED=true MOCK_ADMIT_UNAVAILABLE=1
start_wrangler
BODY="$(stream_request)"
sleep 1
grep -q 'failing closed' <<<"$BODY" && pass "the client got a fail-closed refusal" \
  || { fail "the client was not refused"; echo "$BODY" | head -c 400; }
expect_log "$TMP/p3.log" "POST /admit -> 503" "admission answered 503"
refute_log "$TMP/p3.log" "POST /v1/chat/completions" \
  "the provider was NEVER called (a 503 was not treated as an allow)"

# ---------------------------------------------------------------------------
say "Phase 4 -- over the cap: a platform block is HTTP 200 {allowed:false}"
start_mock "$TMP/p4.log" MOCK_MAX_USD=0.001 MOCK_ENFORCEMENT_MODE=STRICT
start_wrangler
BODY="$(stream_request)"
sleep 1
grep -q 'exceeds the 1d_rolling cap' <<<"$BODY" && pass "the client got the platform's block reason" \
  || { fail "the block reason did not reach the client"; echo "$BODY" | head -c 400; }
expect_log "$TMP/p4.log" "POST /admit -> 200 BLOCKED" "the platform blocked with HTTP 200"
refute_log "$TMP/p4.log" "POST /v1/chat/completions" "the provider was NEVER called"

# ---------------------------------------------------------------------------
say "Result"
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[32mall phases passed\033[0m\n'
else
  printf '\033[31m%d assertion(s) failed\033[0m\n' "$FAILURES"
fi
exit "$FAILURES"
