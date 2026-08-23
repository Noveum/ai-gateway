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
# bundled mock (`novaguard_mock_platform.py`) plays the control plane plus the
# OpenAI and Anthropic upstreams; the Worker is pointed at it with local URL
# overrides.
#
# Usage:  scripts/novaguard_worker_e2e.sh
# Env:    WRANGLER_VERSION (default 4.120.0), GATEWAY_PORT (8787),
#         MOCK_PORT (8899), CONCURRENT_CLIENTS (default 16; 12/16/20),
#         NODE_BIN_DIR (a dir to prepend to PATH for node>=22)
set -uo pipefail

WRANGLER_VERSION="${WRANGLER_VERSION:-4.120.0}"
GATEWAY_PORT="${GATEWAY_PORT:-8787}"
MOCK_PORT="${MOCK_PORT:-8899}"
CONCURRENT_CLIENTS="${CONCURRENT_CLIENTS:-16}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
[ -n "${NODE_BIN_DIR:-}" ] && PATH="$NODE_BIN_DIR:$PATH"
# Keep the caller's tool ordering. CI and release validation intentionally put
# their pinned `worker-build` ahead of any older global Cargo install; silently
# prepending ~/.cargo/bin here used the wrong builder and produced a different
# bundle layout from CI.
export PATH

FAILURES=0
MOCK_PID=""
WRANGLER_PID=""
REQUEST_PIDS=()
REQUEST_LABELS=()

case "$CONCURRENT_CLIENTS" in
  ''|*[!0-9]*) echo "CONCURRENT_CLIENTS must be 12, 16, or 20" >&2; exit 1 ;;
esac
if (( CONCURRENT_CLIENTS < 12 || CONCURRENT_CLIENTS > 20 || CONCURRENT_CLIENTS % 4 != 0 )); then
  echo "CONCURRENT_CLIENTS must be 12, 16, or 20" >&2
  exit 1
fi

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILURES=$((FAILURES + 1)); }

cleanup() {
  local status=$?
  local pid
  trap - EXIT
  # Bash 3.2 (the macOS system shell) treats an empty array expansion as unset
  # under `set -u`, even when the array was explicitly declared.
  if [ -n "${REQUEST_PIDS[*]-}" ]; then
    for pid in "${REQUEST_PIDS[@]}"; do
      kill "$pid" 2>/dev/null
    done
    for pid in "${REQUEST_PIDS[@]}"; do
      wait "$pid" 2>/dev/null
    done
  fi
  [ -n "$WRANGLER_PID" ] && kill "$WRANGLER_PID" 2>/dev/null
  [ -n "$WRANGLER_PID" ] && wait "$WRANGLER_PID" 2>/dev/null
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null
  [ -n "$MOCK_PID" ] && wait "$MOCK_PID" 2>/dev/null
  exit "$status"
}
trap cleanup EXIT

# Assert a mock-side log line exists (the mock logs every request it serves, so
# this is how we observe what the Worker actually did, including the calls that
# happen AFTER the client response is complete).
expect_log()   { grep -qF -- "$2" "$1" && pass "$3" || { fail "$3"; echo "      --- mock log ---"; sed 's/^/      /' "$1"; }; }
refute_log()   { grep -qF -- "$2" "$1" && { fail "$3"; sed 's/^/      /' "$1"; } || pass "$3"; }

json_value() {
  python3 -c '
import json, sys
value = json.loads(sys.argv[1])
for key in sys.argv[2].split("."):
    value = value[key]
print(value)
' "$1" "$2"
}

expect_stat() {
  local stats="$1" path="$2" expected="$3" description="$4" actual
  actual="$(json_value "$stats" "$path" 2>/dev/null)" || actual="<unreadable>"
  [ "$actual" = "$expected" ] \
    && pass "$description ($path=$actual)" \
    || fail "$description (expected $path=$expected, got $actual)"
}

expect_stat_at_least() {
  local stats="$1" path="$2" minimum="$3" description="$4" actual
  actual="$(json_value "$stats" "$path" 2>/dev/null)" || actual="0"
  [[ "$actual" =~ ^[0-9]+$ ]] && (( actual >= minimum )) \
    && pass "$description ($path=$actual)" \
    || fail "$description (expected $path >= $minimum, got $actual)"
}

start_mock() {
  local log="$1"; shift
  if [ -n "$MOCK_PID" ]; then
    kill "$MOCK_PID" 2>/dev/null
    wait "$MOCK_PID" 2>/dev/null
    MOCK_PID=""
  fi
  env MOCK_PORT="$MOCK_PORT" "$@" python3 "$ROOT/scripts/novaguard_mock_platform.py" >"$log" 2>&1 &
  MOCK_PID=$!
  disown "$MOCK_PID" 2>/dev/null   # keep phase restarts from printing job-control noise
  for _ in $(seq 1 40); do
    curl -sf -o /dev/null "http://127.0.0.1:$MOCK_PORT/__mock/stats" && return 0
    sleep 0.25
  done
  echo "mock platform did not come up; see $log" >&2
  exit 1
}

start_wrangler() {
  local openai_base_url="${E2E_OPENAI_BASE_URL:-http://127.0.0.1:$MOCK_PORT}"
  local anthropic_base_url="${E2E_ANTHROPIC_BASE_URL:-http://127.0.0.1:$MOCK_PORT}"
  local upstream_timeout_ms="${E2E_UPSTREAM_TIMEOUT_MS:-600000}"
  local assumed_output_tokens="${E2E_ASSUMED_OUTPUT_TOKENS:-1024}"
  if [ -n "$WRANGLER_PID" ]; then
    kill "$WRANGLER_PID" 2>/dev/null
    wait "$WRANGLER_PID" 2>/dev/null
    WRANGLER_PID=""
  fi
  # A fresh process means a fresh isolate, which is the only way to drop the
  # 60s in-isolate policy cache between phases. Restarting is far cheaper than
  # sleeping it out.
  WRANGLER_SEND_METRICS=false npx --yes "wrangler@${WRANGLER_VERSION}" dev \
    --port "$GATEWAY_PORT" --ip 127.0.0.1 --log-level info \
    --var NOVEUM_API_KEY:mock-key \
    --var NOVEUM_GUARD_PROJECT_ID:proj_mock \
    --var "NOVEUM_API_URL:http://127.0.0.1:$MOCK_PORT" \
    --var "OPENAI_BASE_URL:$openai_base_url" \
    --var "ANTHROPIC_BASE_URL:$anthropic_base_url" \
    --var "NOVEUM_GUARD_WORKER_UPSTREAM_TIMEOUT_MS:$upstream_timeout_ms" \
    --var "NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS:$assumed_output_tokens" \
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
  local agent_id="${1:-phase-stream}"
  curl -sN --max-time 30 "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H "x-user-id: $agent_id" \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","stream":true,"max_tokens":4096,
         "messages":[{"role":"user","content":"say hi"}]}'
}

deadline_stream_request() {
  curl -sN --max-time 5 "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'x-user-id: deadline-stream' \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","stream":true,"max_tokens":64,
         "messages":[{"role":"user","content":"start, then wait"}]}'
}

monotonic_ms() {
  python3 -c 'import time; print(time.monotonic_ns() // 1_000_000)'
}

unbounded_request() {
  curl -s --max-time 30 "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","messages":[{"role":"user","content":"say hi"}]}'
}

unbounded_request_with_status() {
  local body_file="$1"
  curl -sS --max-time 30 -o "$body_file" -w '%{http_code}' \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","messages":[{"role":"user","content":"policy selection"}]}'
}

check_unbounded_policy_case() {
  local slug="$1" expected_status="$2" description="$3" log body_file status body
  shift 3
  log="$TMP/p9-$slug.log"
  body_file="$TMP/p9-$slug.body"

  start_mock "$log" "$@"
  start_wrangler
  status="$(unbounded_request_with_status "$body_file")" || status="transport_error"
  body="$(<"$body_file")"

  if [ "$expected_status" = "400" ]; then
    [ "$status" = "400" ] && grep -q '"code":"missing_output_limit"' <<<"$body" \
      && pass "$description: rejected with missing_output_limit" \
      || { fail "$description: expected HTTP 400 missing_output_limit, got HTTP $status"; echo "$body"; }
    refute_log "$log" "POST /v1/chat/completions" \
      "$description: rejection happened before the provider"
  else
    [ "$status" = "200" ] \
      && grep -q '"id":"chatcmpl-mock-e2e"' <<<"$body" \
      && grep -q '"content":"mock completion"' <<<"$body" \
      && ! grep -q '"x_noveum_guard"' <<<"$body" \
      && pass "$description: unbounded request was allowed" \
      || { fail "$description: expected the mock provider completion, got HTTP $status"; echo "$body"; }
    grep -q '"code":"missing_output_limit"' <<<"$body" \
      && fail "$description: the strict-cap error leaked into an allowed case" \
      || pass "$description: no missing_output_limit rejection"
    expect_log "$log" "POST /v1/chat/completions" \
      "$description: the allowed request reached the provider"
  fi
}

expect_strict_pre_admit_rejection() {
  local slug="$1" provider="$2" path="$3" content_type="$4" payload="$5" description="$6"
  local body_file="$TMP/p9-strict-$slug.body" status body

  if [ "$provider" = "bedrock" ]; then
    status="$(curl -sS --max-time 30 -o "$body_file" -w '%{http_code}' \
      "http://127.0.0.1:$GATEWAY_PORT$path" \
      -H 'x-provider: bedrock' \
      -H 'x-aws-access-key-id: AKIATEST' \
      -H 'x-aws-secret-access-key: worker-test-secret' \
      -H 'x-aws-region: eu-south-1' \
      -H "Content-Type: $content_type" \
      --data-binary "$payload"
    )" || status="transport_error"
  else
    status="$(curl -sS --max-time 30 -o "$body_file" -w '%{http_code}' \
      "http://127.0.0.1:$GATEWAY_PORT$path" \
      -H 'Authorization: Bearer sk-test' \
      -H "x-provider: $provider" \
      -H "Content-Type: $content_type" \
      --data-binary "$payload"
    )" || status="transport_error"
  fi
  body="$(<"$body_file")"

  [ "$status" = "400" ] && grep -q '"code":"unsupported_strict_input"' <<<"$body" \
    && pass "$description: rejected with unsupported_strict_input" \
    || { fail "$description: expected HTTP 400 unsupported_strict_input, got HTTP $status"; echo "$body"; }
}

transformed_admission_request() {
  local with_tools="$1" tools=""
  if [ "$with_tools" = "yes" ]; then
    tools=',"tools":[{"type":"function","function":{"name":"lookup","description":"bounded schema","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}]'
  fi
  curl -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'Content-Type: application/json' \
    -d "{\"model\":\"gpt-4o\",\"max_tokens\":64,\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]$tools}"
}

assert_transformed_admission_estimates() {
  local stats="$1"
  python3 -c '
import json, sys

snapshot = json.loads(sys.argv[1])
admits = snapshot["admitBodies"]
provider = snapshot["providerOpenaiRequests"]
assert len(admits) == 2, f"expected two admissions, got {len(admits)}: {admits!r}"
assert len(provider) == 2, f"expected two upstream requests, got {len(provider)}: {provider!r}"

for index, (admit, upstream, has_tools) in enumerate(zip(admits, provider, (False, True))):
    body = upstream["body"]
    content = body["messages"][0]["content"]
    assert content == "Y" * 8_192, (
        f"request {index} did not forward the expanded content: len={len(content)}"
    )
    assert body["service_tier"] == "default", body
    assert bool(body.get("tools")) is has_tools, body
    raw_bytes = upstream["rawBytes"]
    tool_reserve = 4_096 if has_tools else 0
    estimated = admit["estimatedInputTokens"]
    expected = raw_bytes + tool_reserve
    assert estimated == expected, (
        f"request {index}: expected exact serialized bytes {raw_bytes} "
        f"plus tool reserve {4_096 if has_tools else 0}, got "
        f"{estimated}"
    )
' "$stats"
}

anthropic_stream_request() {
  local agent_id="${1:-phase-anthropic-stream}"
  curl -sN --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions?beta=worker-e2e" \
    -H 'Authorization: Bearer sk-ant-test' -H 'x-provider: anthropic' \
    -H "x-user-id: $agent_id" \
    -H 'Content-Type: application/json' \
    -d '{
      "model":"claude-sonnet-5","stream":true,"max_completion_tokens":64,
      "stream_options":{"include_usage":false},
      "messages":[
        {"role":"system","content":"system-one"},
        {"role":"developer","content":"developer-two"},
        {"role":"user","content":"Use the weather tool"}
      ],
      "tools":[{"type":"function","function":{
        "name":"get_weather","description":"Weather lookup",
        "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
      }}]
    }'
}

anthropic_direct_header_request() {
  curl --http1.1 -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions?beta=worker-e2e" \
    -H 'x-provider: anthropic' \
    -H 'x-api-key: sk-ant-direct-test' \
    -H 'anthropic-beta: worker-e2e-beta' \
    -H 'x-noveum-arbitrary-secret: should-not-forward-x-noveum' \
    -H 'accept-encoding: should-not-forward-accept-encoding' \
    -H 'content-encoding: identity' \
    -H 'connection: keep-alive, x-hop-by-hop-e2e' \
    -H 'x-hop-by-hop-e2e: should-not-forward-connection-token' \
    -H 'keep-alive: should-not-forward-keep-alive' \
    -H 'proxy-authenticate: should-not-forward-proxy-authenticate' \
    -H 'proxy-authorization: Basic c2hvdWxkLW5vdC1mb3J3YXJk' \
    -H 'proxy-connection: should-not-forward-proxy-connection' \
    -H 'te: trailers' \
    -H 'trailer: x-should-not-forward-trailer' \
    -H 'transfer-encoding: chunked' \
    -H 'upgrade: should-not-forward-upgrade' \
    -H 'Content-Type: application/json' \
    -d '{
      "model":"claude-sonnet-5","max_completion_tokens":16,
      "messages":[{"role":"user","content":"header parity"}]
    }'
}

check_invalid_anthropic_auth() {
  local slug="$1" authorization="$2" log="$3" body_file status body model
  body_file="$TMP/p6-auth-$slug.body"
  model="invalid-auth-$slug-should-not-reach"
  status="$(curl -sS --max-time 30 -o "$body_file" -w '%{http_code}' \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'x-provider: anthropic' \
    -H "Authorization: $authorization" \
    -H 'Content-Type: application/json' \
    -d "{\"model\":\"$model\",\"max_completion_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"auth rejection\"}]}"
  )" || status="transport_error"
  body="$(<"$body_file")"

  [ "$status" = "401" ] && grep -q '"type":"authentication_error"' <<<"$body" \
    && pass "$slug Authorization was rejected with an Anthropic-shaped 401" \
    || { fail "$slug Authorization expected HTTP 401 authentication_error, got HTTP $status"; echo "$body"; }
  refute_log "$log" "model=$model" \
    "$slug Authorization was rejected before the Anthropic provider"
}

anthropic_pricing_request() {
  local model="$1" cache_ttl="${2:-}" geo="${3:-global}" cache_field="" geo_field=""
  if [ "$cache_ttl" = "1h" ]; then
    cache_field=',"cache_control":{"type":"ephemeral","ttl":"1h"}'
  fi
  if [ "$geo" = "global" ]; then
    geo_field=',"inference_geo":"global"'
  fi
  curl -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-ant-test' -H 'x-provider: anthropic' \
    -H 'Content-Type: application/json' \
    -d "{\"model\":\"$model\",\"max_tokens\":100$cache_field$geo_field,\"messages\":[{\"role\":\"user\",\"content\":\"123456789012345678901234567890123456789\"}]}"
}

assert_anthropic_admission_pricing() {
  local stats="$1"
  python3 -c '
import json, sys

bodies = json.loads(sys.argv[1])["admitBodies"]
assert len(bodies) == 5, f"expected five admissions, got {len(bodies)}: {bodies!r}"
plain, cached, opus, omitted_geo = bodies[-4:]
for label, body in (
    ("plain", plain),
    ("cached", cached),
    ("opus", opus),
    ("omitted_geo", omitted_geo),
):
    assert body["estimatedInputTokens"] > 0, f"{label} input estimate: {body!r}"
    assert body["maximumOutputTokens"] == 100, f"{label} output ceiling: {body!r}"

# Sonnet 5: $2/M plain input, $4/M one-hour cache write, $10/M output.
expected_plain = (plain["estimatedInputTokens"] / 1_000_000) * 2 + (100 / 1_000_000) * 10
expected_cached = (cached["estimatedInputTokens"] / 1_000_000) * 4 + (100 / 1_000_000) * 10
assert plain["model"] == "claude-sonnet-5", plain
assert abs(plain["estimatedCostUsd"] - expected_plain) < 1e-12, plain
assert cached["model"] == "claude-sonnet-5", cached
assert abs(cached["estimatedCostUsd"] - expected_cached) < 1e-12, cached
assert cached["estimatedCostUsd"] > plain["estimatedCostUsd"], (plain, cached)

# A fail-closed strict cap must recognize the currently active Opus model.
expected_opus = (opus["estimatedInputTokens"] / 1_000_000) * 5 + (100 / 1_000_000) * 25
assert opus["model"] == "claude-opus-5", opus
assert abs(opus["estimatedCostUsd"] - expected_opus) < 1e-12, opus

# Omitted Anthropic geo conservatively reserves the possible 1.1x US-only
# residency premium, including the declared one-hour cache-write dimension.
expected_omitted_geo = (
    (omitted_geo["estimatedInputTokens"] / 1_000_000) * 4 * 1.1
    + (100 / 1_000_000) * 10 * 1.1
)
assert omitted_geo["model"] == "claude-sonnet-5", omitted_geo
assert abs(omitted_geo["estimatedCostUsd"] - expected_omitted_geo) < 1e-12, omitted_geo
assert omitted_geo["estimatedCostUsd"] > cached["estimatedCostUsd"], (cached, omitted_geo)
' "$stats"
}

openai_buffered_request() {
  local agent_id="$1"
  curl -sS --max-time 30 "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H "x-user-id: $agent_id" \
    -H 'Content-Type: application/json' \
    -d '{"model":"gpt-4o","max_tokens":64,
         "messages":[{"role":"user","content":"buffered multi-agent request"}]}'
}

anthropic_buffered_request() {
  local agent_id="$1"
  curl -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions?beta=worker-e2e" \
    -H 'Authorization: Bearer sk-ant-test' -H 'x-provider: anthropic' \
    -H "x-user-id: $agent_id" \
    -H 'Content-Type: application/json' \
    -d '{
      "model":"claude-sonnet-5","max_completion_tokens":64,
      "messages":[
        {"role":"system","content":"concurrent system"},
        {"role":"user","content":"buffered multi-agent request"}
      ]
    }'
}

anthropic_unbounded_request() {
  curl -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-ant-test' -H 'x-provider: anthropic' \
    -H 'Content-Type: application/json' \
    -d '{
      "model":"claude-sonnet-5",
      "messages":[{"role":"user","content":"binding default"}]
    }'
}

cache_race_request() {
  local model="$1"
  curl -sS --max-time 30 \
    "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
    -H 'Content-Type: application/json' \
    -d "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"race-block-me\"}]}"
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
# A cost with no record of the catalog behind it cannot be reproduced once rates
# roll, and SCHEDULED_PRICING rolls them with no deploy.
refute_log "$TMP/p1.log" "pricingVersion=ABSENT" \
  "both the reservation and its settlement carried a pricingVersion"

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
say "Phase 5 -- an unbounded strict cost-cap request is a client 400"
start_mock "$TMP/p5.log" MOCK_MAX_USD=1.0 MOCK_ENFORCEMENT_MODE=STRICT
start_wrangler
BODY="$(unbounded_request)"
grep -q '"code":"missing_output_limit"' <<<"$BODY" \
  && pass "the Worker rejected the unbounded request with missing_output_limit" \
  || { fail "the Worker did not apply the strict output-limit contract"; echo "$BODY"; }
refute_log "$TMP/p5.log" "POST /admit" "no reservation was created for the invalid request"
refute_log "$TMP/p5.log" "POST /v1/chat/completions" "the invalid request never reached the provider"

# ---------------------------------------------------------------------------
say "Phase 6 -- transparent Anthropic stream is normalized, translated, and tool-safe"
start_mock "$TMP/p6.log" MOCK_NO_POLICIES=1 \
  MOCK_STREAM_PROMPT_TOKENS=11 MOCK_STREAM_COMPLETION_TOKENS=4
start_wrangler
BODY="$(anthropic_stream_request)"
grep -q '"tool_calls"' <<<"$BODY" && pass "OpenAI tool_call deltas reached the client" \
  || { fail "Anthropic tool_use was lost from the stream"; echo "$BODY"; }
grep -q 'Paris' <<<"$BODY" && pass "fragmented tool arguments reached the client" \
  || { fail "tool arguments were lost or corrupted"; echo "$BODY"; }
grep -q '"finish_reason":"tool_calls"' <<<"$BODY" \
  && pass "tool_use mapped to finish_reason=tool_calls" \
  || { fail "the translated stream did not finish as tool_calls"; echo "$BODY"; }
grep -q '"prompt_tokens":11' <<<"$BODY" && grep -q '"completion_tokens":4' <<<"$BODY" \
  && pass "terminal Anthropic usage became OpenAI usage" \
  || { fail "translated terminal usage is missing"; echo "$BODY"; }
grep -q 'data: \[DONE\]' <<<"$BODY" && pass "the translated stream terminated with [DONE]" \
  || { fail "the translated stream did not terminate cleanly"; echo "$BODY"; }
grep -q 'event: message_start' <<<"$BODY" \
  && fail "raw Anthropic events leaked to an OpenAI client" \
  || pass "no raw Anthropic event names leaked"
expect_log "$TMP/p6.log" "POST /v1/messages?beta=worker-e2e" \
  "Anthropic query string reached the native Messages route"
expect_log "$TMP/p6.log" "max_tokens=64" "max_completion_tokens mapped to max_tokens"
expect_log "$TMP/p6.log" "max_completion_present=False stream_options_present=False" \
  "OpenAI-only fields were stripped before Anthropic"
expect_log "$TMP/p6.log" "roles=['user'] tools=['get_weather']" \
  "system/developer roles were hoisted and the tool schema was mapped"
expect_log "$TMP/p6.log" "system='system-one\\ndeveloper-two'" \
  "system and developer instructions retained their order"
expect_log "$TMP/p6.log" "x_api_key=True" "Bearer auth became Anthropic x-api-key auth"
refute_log "$TMP/p6.log" "POST /admit" "the no-policy request did not create a reservation"

DIRECT_BODY="$(anthropic_direct_header_request)"
grep -q '"choices"' <<<"$DIRECT_BODY" \
  && pass "a direct Anthropic x-api-key completed through the Worker" \
  || { fail "the direct Anthropic x-api-key request failed"; echo "$DIRECT_BODY"; }
expect_log "$TMP/p6.log" "anthropic_beta='worker-e2e-beta'" \
  "anthropic-beta reached the native Messages upstream unchanged"
expect_log "$TMP/p6.log" "filtered_header_leaks=[]" \
  "x-noveum, hop-by-hop, proxy-auth, encoding, and Connection-nominated headers were stripped"
expect_log "$TMP/p6.log" "x_api_key_marker=direct-test" \
  "the caller's direct x-api-key reached Anthropic"

check_invalid_anthropic_auth "Basic" "Basic c2stdGVzdA==" "$TMP/p6.log"
check_invalid_anthropic_auth "malformed-Bearer" "Bearer one two" "$TMP/p6.log"

# ---------------------------------------------------------------------------
say "Phase 7 -- guarded Anthropic stream settles after translation"
start_mock "$TMP/p7.log" MOCK_MAX_USD=1.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_STREAM_PROMPT_TOKENS=11 MOCK_STREAM_COMPLETION_TOKENS=4
start_wrangler
BODY="$(anthropic_stream_request)"
grep -q 'data: \[DONE\]' <<<"$BODY" && pass "guarded Anthropic stream completed" \
  || { fail "guarded Anthropic stream failed"; echo "$BODY"; }
expect_log "$TMP/p7.log" "POST /admit -> 200 ALLOWED" "Anthropic request reserved atomically"
expect_log "$TMP/p7.log" "/complete -> 202  in=11 out=4" \
  "Anthropic stream settled at authoritative usage through waitUntil"
refute_log "$TMP/p7.log" "/abandon" "translated Anthropic usage prevented estimate retention"

PLAIN_PRICING_BODY="$(anthropic_pricing_request claude-sonnet-5)"
CACHE_PRICING_BODY="$(anthropic_pricing_request claude-sonnet-5 1h)"
OPUS_PRICING_BODY="$(anthropic_pricing_request claude-opus-5)"
OMITTED_GEO_BODY="$(anthropic_pricing_request claude-sonnet-5 1h omitted)"
grep -q '"choices"' <<<"$PLAIN_PRICING_BODY" \
  && grep -q '"choices"' <<<"$CACHE_PRICING_BODY" \
  && pass "plain and one-hour-cache Sonnet requests reached Anthropic under a strict cap" \
  || { fail "a Sonnet admission-pricing probe failed"; echo "$PLAIN_PRICING_BODY"; echo "$CACHE_PRICING_BODY"; }
grep -q '"choices"' <<<"$OPUS_PRICING_BODY" \
  && pass "strict fail-closed policy recognized active model claude-opus-5" \
  || { fail "claude-opus-5 was rejected as unpriceable or failed upstream"; echo "$OPUS_PRICING_BODY"; }
grep -q '"choices"' <<<"$OMITTED_GEO_BODY" \
  && pass "omitted-geo Anthropic cache probe reached the provider" \
  || { fail "omitted-geo Anthropic cache probe failed"; echo "$OMITTED_GEO_BODY"; }
PRICING_STATS="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || PRICING_STATS=""
if [ -n "$PRICING_STATS" ] && assert_anthropic_admission_pricing "$PRICING_STATS"; then
  pass "Worker /admit prices plain, one-hour-cache, and Opus requests exactly"
else
  fail "Worker /admit did not preserve Anthropic request-pricing parity"
fi

# ---------------------------------------------------------------------------
say "Phase 8 -- concurrent multi-agent traffic settles exactly once per request"
start_mock "$TMP/p8.log" MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_PROMPT_TOKENS=11 MOCK_COMPLETION_TOKENS=4 \
  MOCK_STREAM_PROMPT_TOKENS=11 MOCK_STREAM_COMPLETION_TOKENS=4 \
  MOCK_STREAM_DELAY_MS=20 MOCK_PROVIDER_DELAY_MS=100
start_wrangler

REQUEST_PIDS=()
REQUEST_LABELS=()
for agent_number in $(seq 1 "$CONCURRENT_CLIENTS"); do
  agent_id="worker-agent-$agent_number"
  output="$TMP/p8-agent-$agent_number.out"
  case $(((agent_number - 1) % 4)) in
    0)
      label="OpenAI buffered $agent_id"
      (openai_buffered_request "$agent_id" >"$output") &
      ;;
    1)
      label="OpenAI streaming $agent_id"
      (stream_request "$agent_id" >"$output") &
      ;;
    2)
      label="Anthropic buffered $agent_id"
      (anthropic_buffered_request "$agent_id" >"$output") &
      ;;
    3)
      label="Anthropic streaming $agent_id"
      (anthropic_stream_request "$agent_id" >"$output") &
      ;;
  esac
  REQUEST_PIDS+=("$!")
  REQUEST_LABELS+=("$label")
done

for request_index in "${!REQUEST_PIDS[@]}"; do
  if ! wait "${REQUEST_PIDS[$request_index]}"; then
    fail "${REQUEST_LABELS[$request_index]} returned a transport error"
  fi
done
# Every client has been reaped; clearing the list prevents cleanup from ever
# signalling a recycled PID later in the script.
REQUEST_PIDS=()

for agent_number in $(seq 1 "$CONCURRENT_CLIENTS"); do
  output="$TMP/p8-agent-$agent_number.out"
  case $(((agent_number - 1) % 4)) in
    0|2)
      grep -q '"choices"' "$output" \
        || { fail "buffered worker-agent-$agent_number did not receive an OpenAI response"; sed 's/^/      /' "$output"; }
      ;;
    1|3)
      grep -q 'data: \[DONE\]' "$output" \
        || { fail "streaming worker-agent-$agent_number did not reach [DONE]"; sed 's/^/      /' "$output"; }
      ;;
  esac
done

# Completion is scheduled through ctx.waitUntil and can land just after the last
# client body closes. Poll the mock's locked snapshot instead of sleeping a
# guessed duration, then assert every cardinality exactly.
STATS=""
for _ in $(seq 1 120); do
  candidate="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || candidate=""
  if [ -n "$candidate" ]; then
    STATS="$candidate"
    completed="$(json_value "$STATS" reservations.completed 2>/dev/null)" || completed=""
    active="$(json_value "$STATS" reservations.active 2>/dev/null)" || active=""
    if [ "$completed" = "$CONCURRENT_CLIENTS" ] && [ "$active" = "0" ]; then
      break
    fi
  fi
  sleep 0.25
done

if [ -z "$STATS" ]; then
  fail "the mock statistics endpoint never answered"
else
  per_kind=$((CONCURRENT_CLIENTS / 4))
  expect_stat "$STATS" counters.admitRequests "$CONCURRENT_CLIENTS" \
    "every concurrent client reached atomic admission"
  expect_stat "$STATS" counters.admitNewAllowed "$CONCURRENT_CLIENTS" \
    "every concurrent client received one new reservation"
  expect_stat "$STATS" counters.admitBlocked 0 "no concurrent client was cap-blocked"
  expect_stat "$STATS" counters.admitReplayed 0 "no client duplicated an admission key"

  expect_stat "$STATS" counters.providerOpenaiBuffered "$per_kind" \
    "all OpenAI buffered calls reached the provider"
  expect_stat "$STATS" counters.providerOpenaiStreaming "$per_kind" \
    "all OpenAI streams reached the provider"
  expect_stat "$STATS" counters.providerAnthropicBuffered "$per_kind" \
    "all Anthropic buffered calls reached the provider"
  expect_stat "$STATS" counters.providerAnthropicStreaming "$per_kind" \
    "all Anthropic streams reached the provider"
  expect_stat "$STATS" counters.providerInFlight 0 "all provider calls finished"
  expect_stat_at_least "$STATS" counters.providerMaxInFlight 2 \
    "the mock observed overlapping provider calls"

  expect_stat "$STATS" counters.settlementRequests "$CONCURRENT_CLIENTS" \
    "every reservation received exactly one settlement request"
  expect_stat "$STATS" counters.settlementCompleted "$CONCURRENT_CLIENTS" \
    "every reservation completed with authoritative usage"
  expect_stat "$STATS" counters.settlementAbandoned 0 "no reservation was abandoned"
  expect_stat "$STATS" counters.settlementCancelled 0 "no admitted request was cancelled"
  expect_stat "$STATS" counters.settlementIdempotent 0 \
    "no settlement was duplicated or retried"
  expect_stat "$STATS" counters.httpErrors 0 "the mock returned no HTTP errors"

  expect_stat "$STATS" reservations.total "$CONCURRENT_CLIENTS" \
    "the reservation ledger contains every client"
  expect_stat "$STATS" reservations.active 0 "the reservation ledger has no active hold"
  expect_stat "$STATS" reservations.completed "$CONCURRENT_CLIENTS" \
    "the reservation ledger completed every client"
  expect_stat "$STATS" reservations.abandoned 0 "the ledger contains no abandonment"
  expect_stat "$STATS" reservations.cancelled 0 "the ledger contains no cancellation"
  expect_stat "$STATS" reservations.reservedUsd 0.0 "all estimated holds were released"
  expect_stat "$STATS" settlementRecords "$CONCURRENT_CLIENTS" \
    "the settlement audit contains exactly one record per client"
fi
refute_log "$TMP/p8.log" "/abandon" "no abandonment endpoint appeared in the mock log"
refute_log "$TMP/p8.log" "Traceback" "the concurrent mock served without a traceback"
refute_log "$TMP/p8.log" "Exception occurred" "the mock had no request-thread exception"
refute_log "$TMP/wrangler.log" "panicked at" "workerd reported no Rust panic"

# ---------------------------------------------------------------------------
say "Phase 9 -- strict policy selection and bounded-input admission invariants"
# This matrix runs the real policy payload through worker::Fetch, platform
# translation, PolicyEngine compilation and finally the Worker request path.
# Every case uses the same unbounded gpt-4o request; only policy semantics vary.
check_unbounded_policy_case "strict-block" 400 \
  "enforcing blocking strict cap" \
  MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT
check_unbounded_policy_case "advisory" 200 \
  "advisory blocking cap" \
  MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=ADVISORY
check_unbounded_policy_case "shadow-strict" 200 \
  "shadow strict cap" \
  MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT MOCK_POLICY_MODE=SHADOW
check_unbounded_policy_case "scope-nonmatch" 200 \
  "strict cap scoped away from gpt-4o" \
  MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_SCOPE_TO_MODELS=claude-sonnet-5
check_unbounded_policy_case "scope-match" 400 \
  "strict cap explicitly scoped to gpt-4o" \
  MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT MOCK_SCOPE_TO_MODELS=GPT-4O

# Admission must measure the exact post-policy body that reaches the provider.
# Run the same one-byte prompt with and without a client function tool: the
# regex policy expands it to 8 KiB, while only the tool-bearing request gets the
# separate 4,096-token provider-injected tool-prompt reserve.
start_mock "$TMP/p9-transformed.log" MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_EXPAND_REDACT_WITH_LEN=8192 MOCK_PROMPT_TOKENS=11 MOCK_COMPLETION_TOKENS=4
start_wrangler
TRANSFORMED_NO_TOOLS="$(transformed_admission_request no)"
TRANSFORMED_WITH_TOOLS="$(transformed_admission_request yes)"
grep -q '"choices"' <<<"$TRANSFORMED_NO_TOOLS" \
  && grep -q '"choices"' <<<"$TRANSFORMED_WITH_TOOLS" \
  && pass "post-transform admission probes reached the mock provider" \
  || { fail "a post-transform admission probe failed"; echo "$TRANSFORMED_NO_TOOLS"; echo "$TRANSFORMED_WITH_TOOLS"; }

TRANSFORM_STATS=""
for _ in $(seq 1 80); do
  candidate="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || candidate=""
  if [ -n "$candidate" ]; then
    TRANSFORM_STATS="$candidate"
    completed="$(json_value "$TRANSFORM_STATS" reservations.completed 2>/dev/null)" || completed=""
    [ "$completed" = "2" ] && break
  fi
  sleep 0.25
done
if [ -n "$TRANSFORM_STATS" ] && assert_transformed_admission_estimates "$TRANSFORM_STATS"; then
  pass "strict admission used forwarded serialized bytes and added 4,096 only for client tools"
else
  fail "strict admission estimate did not match the transformed upstream body"
fi
if [ -n "$TRANSFORM_STATS" ]; then
  expect_stat "$TRANSFORM_STATS" counters.admitRequests 2 \
    "both transformed requests were admitted exactly once"
  expect_stat "$TRANSFORM_STATS" counters.providerOpenaiBuffered 2 \
    "both transformed requests reached the OpenAI-compatible provider"
  expect_stat "$TRANSFORM_STATS" reservations.completed 2 \
    "both transformed-request reservations settled before the next phase"
fi

# One strict Worker instance exercises every unsupported surface. Each request
# must fail locally with the same explicit 400, and the aggregate mock snapshot
# proves none created even a transient reservation or provider call.
start_mock "$TMP/p9-unsupported.log" MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT
start_wrangler
expect_strict_pre_admit_rejection "malformed-json" openai "/v1/chat/completions" \
  "application/json" '{not-json' \
  "malformed JSON under a strict cap"
expect_strict_pre_admit_rejection "non-json" openai "/v1/chat/completions" \
  "text/plain" 'opaque request bytes' \
  "non-JSON input under a strict cap"
expect_strict_pre_admit_rejection "responses" openai "/v1/responses" \
  "application/json" \
  '{"model":"gpt-4o","max_output_tokens":64,"input":"server-side context"}' \
  "/v1/responses under a strict cap"
expect_strict_pre_admit_rejection "mcp" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"bounded"}],"mcp_servers":[{"type":"url","url":"https://mcp.invalid"}]}' \
  "remote MCP input under a strict cap"
expect_strict_pre_admit_rejection "web-search" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"bounded"}],"web_search_options":{"search_context_size":"high"}}' \
  "provider web search under a strict cap"
expect_strict_pre_admit_rejection "remote-image" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"bounded"},{"type":"image_url","image_url":{"url":"https://media.invalid/image.png"}}]}]}' \
  "remote image input under a strict cap"
expect_strict_pre_admit_rejection "audio" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"modalities":["text","audio"],"messages":[{"role":"user","content":"bounded"}]}' \
  "audio output under a strict cap"
expect_strict_pre_admit_rejection "perplexity" perplexity "/v1/chat/completions" \
  "application/json" \
  '{"model":"sonar","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "Perplexity request-fee surface under a strict cap"
expect_strict_pre_admit_rejection "openrouter" openrouter "/v1/chat/completions" \
  "application/json" \
  '{"model":"openai/gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "OpenRouter routed-model surface under a strict cap"
expect_strict_pre_admit_rejection "service-tier" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"service_tier":"priority","messages":[{"role":"user","content":"bounded"}]}' \
  "non-default OpenAI service tier under a strict cap"
expect_strict_pre_admit_rejection "groq-compound" groq "/v1/chat/completions" \
  "application/json" \
  '{"model":"groq/compound","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "Groq Compound built-in tools under a strict cap"
expect_strict_pre_admit_rejection "xai-search-parameters" xai "/v1/chat/completions" \
  "application/json" \
  '{"model":"grok-4.3","max_tokens":64,"messages":[{"role":"user","content":"bounded"}],"search_parameters":{"mode":"on"}}' \
  "xAI server-side search under a strict cap"
expect_strict_pre_admit_rejection "bedrock-region-priced-nova" bedrock "/v1/chat/completions" \
  "application/json" \
  '{"model":"amazon.nova-pro-v1:0","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "source-region-priced Bedrock Nova under a strict cap"
expect_strict_pre_admit_rejection "divergent-output-limits" groq "/v1/chat/completions" \
  "application/json" \
  '{"model":"openai/gpt-oss-20b","max_tokens":64,"max_output_tokens":65,"messages":[{"role":"user","content":"bounded"}]}' \
  "conflicting output-limit aliases under a strict cap"
expect_strict_pre_admit_rejection "gemini-search" gemini "/v1/chat/completions" \
  "application/json" \
  '{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"bounded"}],"tools":[{"google_search":{}}]}' \
  "Gemini Google Search under a strict cap"
expect_strict_pre_admit_rejection "gemini-cached-content" google "/v1/chat/completions" \
  "application/json" \
  '{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"bounded"}],"extra_body":{"google":{"cached_content":"cachedContents/e2e"}}}' \
  "Gemini provider-cached context under a strict cap"
expect_strict_pre_admit_rejection "mistral-document" mistral "/v1/chat/completions" \
  "application/json" \
  '{"model":"mistral-medium-latest","max_tokens":64,"messages":[{"role":"user","content":[{"type":"document_url","document_url":"https://media.invalid/large.pdf"}]}]}' \
  "Mistral document URL under a strict cap"
expect_strict_pre_admit_rejection "together-video" together "/v1/chat/completions" \
  "application/json" \
  '{"model":"Qwen/Qwen2.5-VL-72B-Instruct","max_tokens":64,"messages":[{"role":"user","content":[{"type":"video_url","video_url":{"url":"https://media.invalid/large.mp4"}}]}]}' \
  "Together video URL under a strict cap"

UNSUPPORTED_STATS="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || UNSUPPORTED_STATS=""
if [ -z "$UNSUPPORTED_STATS" ]; then
  fail "the mock statistics endpoint did not answer after strict-input rejections"
else
  expect_stat "$UNSUPPORTED_STATS" counters.admitRequests 0 \
    "unsupported strict inputs never reached /admit"
  expect_stat "$UNSUPPORTED_STATS" counters.providerOpenaiBuffered 0 \
    "unsupported strict inputs never reached buffered OpenAI upstream"
  expect_stat "$UNSUPPORTED_STATS" counters.providerOpenaiStreaming 0 \
    "unsupported strict inputs never reached streaming OpenAI upstream"
  expect_stat "$UNSUPPORTED_STATS" counters.providerAnthropicBuffered 0 \
    "unsupported strict inputs never reached buffered Anthropic upstream"
  expect_stat "$UNSUPPORTED_STATS" counters.providerAnthropicStreaming 0 \
    "unsupported strict inputs never reached streaming Anthropic upstream"
  expect_stat "$UNSUPPORTED_STATS" reservations.total 0 \
    "unsupported strict inputs created no reservations"
  expect_stat "$UNSUPPORTED_STATS" settlementRecords 0 \
    "unsupported strict inputs created no settlement records"
fi
refute_log "$TMP/p9-unsupported.log" "POST /admit" \
  "the strict-input rejection matrix made zero admission calls"
refute_log "$TMP/p9-unsupported.log" "(mock provider)" \
  "the strict-input rejection matrix made zero provider calls"

# Strict catalog rates are valid only for the first-party OpenAI/Anthropic
# endpoints. Loopback overrides are deliberately allowed for this harness, so
# use non-loopback compatible endpoints in a fresh isolate and prove rejection
# still happens before admission or network dispatch.
start_mock "$TMP/p9-custom-base.log" MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT
E2E_OPENAI_BASE_URL="https://openai-compatible.invalid/v1"
E2E_ANTHROPIC_BASE_URL="https://anthropic-compatible.invalid"
start_wrangler
unset E2E_OPENAI_BASE_URL E2E_ANTHROPIC_BASE_URL
expect_strict_pre_admit_rejection "openai-custom-base" openai "/v1/chat/completions" \
  "application/json" \
  '{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "custom OpenAI-compatible base URL under a strict cap"
expect_strict_pre_admit_rejection "anthropic-custom-base" anthropic "/v1/chat/completions" \
  "application/json" \
  '{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"bounded"}]}' \
  "custom Anthropic-compatible base URL under a strict cap"
CUSTOM_BASE_STATS="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || CUSTOM_BASE_STATS=""
if [ -z "$CUSTOM_BASE_STATS" ]; then
  fail "the mock statistics endpoint did not answer after custom-base rejections"
else
  expect_stat "$CUSTOM_BASE_STATS" counters.admitRequests 0 \
    "custom provider bases never reached /admit"
  expect_stat "$CUSTOM_BASE_STATS" counters.providerOpenaiBuffered 0 \
    "custom provider bases never reached the OpenAI mock"
  expect_stat "$CUSTOM_BASE_STATS" counters.providerAnthropicBuffered 0 \
    "custom provider bases never reached the Anthropic mock"
  expect_stat "$CUSTOM_BASE_STATS" reservations.total 0 \
    "custom provider bases created no reservations"
fi
refute_log "$TMP/p9-custom-base.log" "POST /admit" \
  "custom-base rejection made zero admission calls"
refute_log "$TMP/p9-custom-base.log" "(mock provider)" \
  "custom-base rejection made zero OpenAI provider calls"
refute_log "$TMP/p9-custom-base.log" "(mock anthropic)" \
  "custom-base rejection made zero Anthropic provider calls"

# ---------------------------------------------------------------------------
say "Phase 10 -- a rate-only policy cannot be bypassed with opaque input"
start_mock "$TMP/p10-rate-opaque.log" MOCK_POLICY_TYPE=RATE_LIMIT MOCK_MAX_REQUESTS=1
start_wrangler
RATE_OPAQUE_BODY="$TMP/p10-rate-opaque.body"
RATE_OPAQUE_STATUS="$(curl -sS --max-time 30 -o "$RATE_OPAQUE_BODY" -w '%{http_code}' \
  "http://127.0.0.1:$GATEWAY_PORT/v1/chat/completions" \
  -H 'Authorization: Bearer sk-test' -H 'x-provider: openai' \
  -H 'Content-Type: multipart/form-data; boundary=test' \
  --data-binary $'--test\r\nopaque\r\n--test--\r\n')" || RATE_OPAQUE_STATUS="transport_error"
[ "$RATE_OPAQUE_STATUS" = "400" ] \
  && grep -q '"code":"unsupported_stateful_input"' "$RATE_OPAQUE_BODY" \
  && pass "rate-only opaque input was rejected deterministically" \
  || { fail "rate-only opaque input expected HTTP 400 unsupported_stateful_input, got HTTP $RATE_OPAQUE_STATUS"; cat "$RATE_OPAQUE_BODY"; }
RATE_OPAQUE_STATS="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || RATE_OPAQUE_STATS=""
if [ -z "$RATE_OPAQUE_STATS" ]; then
  fail "the mock statistics endpoint did not answer after the rate-only rejection"
else
  expect_stat "$RATE_OPAQUE_STATS" counters.admitRequests 0 \
    "opaque rate-only input was rejected before /admit"
  expect_stat "$RATE_OPAQUE_STATS" counters.providerOpenaiBuffered 0 \
    "opaque rate-only input never reached the provider"
  expect_stat "$RATE_OPAQUE_STATS" reservations.total 0 \
    "opaque rate-only input created no reservation"
fi
refute_log "$TMP/p10-rate-opaque.log" "POST /admit" \
  "rate-only opaque rejection made zero admission calls"
refute_log "$TMP/p10-rate-opaque.log" "(mock provider)" \
  "rate-only opaque rejection made zero provider calls"

# Admission is authoritative for rate limits too. A 503 must take the rate
# policy's own failClosed decision, independent of cost-cap enforcement mode.
start_mock "$TMP/p10-rate-503-closed.log" MOCK_POLICY_TYPE=RATE_LIMIT \
  MOCK_MAX_REQUESTS=1 MOCK_FAIL_CLOSED=true MOCK_ADMIT_UNAVAILABLE=1
start_wrangler
RATE_CLOSED_BODY="$(openai_buffered_request rate-admit-closed)"
grep -q 'failing closed' <<<"$RATE_CLOSED_BODY" \
  && pass "rate-only admission 503 honored failClosed=true" \
  || { fail "rate-only failClosed=true did not refuse the request"; echo "$RATE_CLOSED_BODY" | head -c 400; }
expect_log "$TMP/p10-rate-503-closed.log" "POST /admit -> 503" \
  "rate-only fail-closed request reached atomic admission"
refute_log "$TMP/p10-rate-503-closed.log" "(mock provider)" \
  "rate-only failClosed=true prevented provider execution"

start_mock "$TMP/p10-rate-503-open.log" MOCK_POLICY_TYPE=RATE_LIMIT \
  MOCK_MAX_REQUESTS=1 MOCK_FAIL_CLOSED=false MOCK_ADMIT_UNAVAILABLE=1
start_wrangler
RATE_OPEN_BODY="$(openai_buffered_request rate-admit-open)"
grep -q '"choices"' <<<"$RATE_OPEN_BODY" \
  && pass "rate-only admission 503 honored failClosed=false" \
  || { fail "rate-only failClosed=false did not reach the provider"; echo "$RATE_OPEN_BODY" | head -c 400; }
expect_log "$TMP/p10-rate-503-open.log" "POST /admit -> 503" \
  "rate-only fail-open request reached atomic admission"
expect_log "$TMP/p10-rate-503-open.log" "(mock provider)" \
  "rate-only failClosed=false allowed provider execution"

# ---------------------------------------------------------------------------
say "Phase 11 -- Worker deadline abandons a live stream before its platform lease"
start_mock "$TMP/p11.log" MOCK_MAX_USD=10.0 MOCK_ENFORCEMENT_MODE=STRICT \
  MOCK_RESERVATION_LEASE_MS=3000 MOCK_STREAM_DELAY_MS=1000 \
  MOCK_STREAM_PROMPT_TOKENS=11 MOCK_STREAM_COMPLETION_TOKENS=4
E2E_UPSTREAM_TIMEOUT_MS=300
start_wrangler
unset E2E_UPSTREAM_TIMEOUT_MS

LEASE_STARTED_MS="$(monotonic_ms)"
LEASE_CURL_EXIT=0
LEASE_STREAM_BODY="$(deadline_stream_request)" || LEASE_CURL_EXIT=$?
grep -q 'chat.completion.chunk' <<<"$LEASE_STREAM_BODY" \
  && pass "the admitted slow stream began reaching the client" \
  || { fail "the slow stream never reached the client before its deadline"; echo "$LEASE_STREAM_BODY"; }
grep -q 'data: \[DONE\]' <<<"$LEASE_STREAM_BODY" \
  && fail "the deliberately slow stream incorrectly ran through [DONE]" \
  || pass "the Worker cut off the slow stream before [DONE] (curl=$LEASE_CURL_EXIT)"

LEASE_STATS=""
for _ in $(seq 1 30); do
  candidate="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || candidate=""
  if [ -n "$candidate" ]; then
    LEASE_STATS="$candidate"
    abandoned="$(json_value "$LEASE_STATS" reservations.abandoned 2>/dev/null)" || abandoned=""
    active="$(json_value "$LEASE_STATS" reservations.active 2>/dev/null)" || active=""
    [ "$abandoned" = "1" ] && [ "$active" = "0" ] && break
  fi
  sleep 0.05
done
LEASE_ABANDONED_MS="$(monotonic_ms)"
LEASE_ELAPSED_MS=$((LEASE_ABANDONED_MS - LEASE_STARTED_MS))
if [ -n "$LEASE_STATS" ]; then
  expect_stat "$LEASE_STATS" counters.admitRequests 1 \
    "the slow stream was admitted exactly once"
  expect_stat "$LEASE_STATS" counters.settlementAbandoned 1 \
    "the deadline produced one conservative abandonment"
  expect_stat "$LEASE_STATS" counters.reservationReaped 0 \
    "the platform did not reap a live reservation"
  expect_stat "$LEASE_STATS" reservations.active 0 \
    "the deadline left no active reservation"
  expect_stat "$LEASE_STATS" reservations.abandoned 1 \
    "the expired client stream retained its estimate as abandoned"
  expect_stat "$LEASE_STATS" reservations.expired 0 \
    "the reservation never reached platform lease expiry"
else
  fail "the mock statistics endpoint did not answer after the Worker deadline"
fi
if (( LEASE_ELAPSED_MS < 3000 )); then
  pass "abandon completed before the 3,000ms platform lease (${LEASE_ELAPSED_MS}ms)"
else
  fail "abandon missed the 3,000ms platform lease (${LEASE_ELAPSED_MS}ms)"
fi
expect_log "$TMP/p11.log" "/abandon -> 202" \
  "the deadline settled through the platform abandon endpoint"

# A second request starts before the first lease could expire. It must obtain a
# fresh reservation because the first is already abandoned, never because the
# platform reaped a hold from underneath a still-live provider call.
LEASE_FOLLOWUP_BODY="$(openai_buffered_request deadline-followup)"
LEASE_FOLLOWUP_MS="$(monotonic_ms)"
LEASE_FOLLOWUP_ELAPSED_MS=$((LEASE_FOLLOWUP_MS - LEASE_STARTED_MS))
grep -q '"choices"' <<<"$LEASE_FOLLOWUP_BODY" \
  && pass "a subsequent bounded request admitted normally" \
  || { fail "the post-deadline bounded request failed"; echo "$LEASE_FOLLOWUP_BODY"; }
if (( LEASE_FOLLOWUP_ELAPSED_MS < 3000 )); then
  pass "the follow-up admitted before the first lease could expire (${LEASE_FOLLOWUP_ELAPSED_MS}ms)"
else
  fail "the follow-up missed the first reservation lease (${LEASE_FOLLOWUP_ELAPSED_MS}ms)"
fi
LEASE_FINAL_STATS=""
for _ in $(seq 1 80); do
  candidate="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || candidate=""
  if [ -n "$candidate" ]; then
    LEASE_FINAL_STATS="$candidate"
    completed="$(json_value "$LEASE_FINAL_STATS" reservations.completed 2>/dev/null)" || completed=""
    active="$(json_value "$LEASE_FINAL_STATS" reservations.active 2>/dev/null)" || active=""
    [ "$completed" = "1" ] && [ "$active" = "0" ] && break
  fi
  sleep 0.05
done
if [ -z "$LEASE_FINAL_STATS" ]; then
  fail "the mock statistics endpoint did not answer after the follow-up admission"
else
  expect_stat "$LEASE_FINAL_STATS" counters.admitRequests 2 \
    "deadline and follow-up each made one admission"
  expect_stat "$LEASE_FINAL_STATS" counters.admitNewAllowed 2 \
    "the follow-up received its own new reservation"
  expect_stat "$LEASE_FINAL_STATS" counters.reservationReaped 0 \
    "the follow-up admission did not depend on lease reaping"
  expect_stat "$LEASE_FINAL_STATS" counters.settlementAbandoned 1 \
    "only the deadline stream was abandoned"
  expect_stat "$LEASE_FINAL_STATS" counters.settlementCompleted 1 \
    "the follow-up completed with authoritative usage"
  expect_stat "$LEASE_FINAL_STATS" reservations.active 0 \
    "the follow-up left no active hold"
  expect_stat "$LEASE_FINAL_STATS" reservations.expired 0 \
    "neither reservation expired underneath a live call"
fi
refute_log "$TMP/p11.log" "REAP reservation=" \
  "no active reservation was reaped during the deadline regression"

# ---------------------------------------------------------------------------
say "Phase 12 -- Worker output-token binding drives admission and Anthropic defaults"
start_mock "$TMP/p12.log" MOCK_MAX_USD=100.0 MOCK_ENFORCEMENT_MODE=ADVISORY \
  MOCK_PROMPT_TOKENS=11 MOCK_COMPLETION_TOKENS=4
E2E_ASSUMED_OUTPUT_TOKENS=128000
start_wrangler
unset E2E_ASSUMED_OUTPUT_TOKENS
BODY="$(anthropic_unbounded_request)"
grep -q '"choices"' <<<"$BODY" \
  && pass "unbounded Anthropic request completed with the Worker binding default" \
  || { fail "the output-token binding probe failed"; echo "$BODY"; }
OUTPUT_BINDING_STATS="$(curl -sf "http://127.0.0.1:$MOCK_PORT/__mock/stats")" || OUTPUT_BINDING_STATS=""
if [ -n "$OUTPUT_BINDING_STATS" ] && python3 -c '
import json, sys
admits = json.loads(sys.argv[1])["admitBodies"]
assert len(admits) == 1, admits
assert admits[0]["maximumOutputTokens"] == 128000, admits[0]
' "$OUTPUT_BINDING_STATS"; then
  pass "Worker /admit used maximumOutputTokens=128000"
else
  fail "Worker /admit ignored NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS=128000"
fi
expect_log "$TMP/p12.log" "max_tokens=128000" \
  "outbound Anthropic request used max_tokens=128000"

# ---------------------------------------------------------------------------
say "Phase 13 -- a delayed old policy refresh cannot roll back a faster new one"
start_mock "$TMP/p13.log" MOCK_POLICY_RACE=1 MOCK_POLICY_RACE_OLD_DELAY_MS=500
start_wrangler
cache_race_request cache-race-first >"$TMP/p13-first.body" &
RACE_FIRST_PID=$!
sleep 0.05
cache_race_request cache-race-second >"$TMP/p13-second.body" &
RACE_SECOND_PID=$!
wait "$RACE_FIRST_PID" || fail "the delayed cache-race request failed"
wait "$RACE_SECOND_PID" || fail "the fast cache-race request failed"
RACE_FINAL_BODY="$(cache_race_request cache-race-final)"
grep -q '"x_noveum_guard"' <<<"$RACE_FINAL_BODY" \
  && pass "the newer blocking policy remained in the isolate cache" \
  || { fail "the delayed old response rolled the policy cache back"; echo "$RACE_FINAL_BODY"; }
expect_log "$TMP/p13.log" "GET /effective -> 200 OLD after 500ms delay" \
  "the old refresh was deterministically delayed"
expect_log "$TMP/p13.log" "GET /effective -> 200 NEW immediately" \
  "the newer refresh completed first"
refute_log "$TMP/p13.log" "model=cache-race-final" \
  "the final probe was blocked before reaching the provider"

# ---------------------------------------------------------------------------
say "Result"
if [ "$FAILURES" -eq 0 ]; then
  printf '\033[32mall phases passed\033[0m\n'
else
  printf '\033[31m%d assertion(s) failed\033[0m\n' "$FAILURES"
fi
exit "$FAILURES"
