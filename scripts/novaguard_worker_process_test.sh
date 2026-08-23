#!/usr/bin/env bash
# Regression test for the mock-restart barrier used by the Workerd E2E suite.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=novaguard_worker_process.sh
source "$ROOT/scripts/novaguard_worker_process.sh"

TMP="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  local status=$?
  trap - EXIT
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf -- "$TMP"
  exit "$status"
}
trap cleanup EXIT

# Allocate a loopback port, then run a listener that deliberately keeps the
# socket open briefly after SIGTERM. `disown` makes job-table state the wrong
# thing to use as the lifecycle contract; the production barrier must observe
# both the process and the port.
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
python3 - "$PORT" >"$TMP/server.log" 2>&1 <<'PY' &
import signal
import socket
import sys
import time

listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", int(sys.argv[1])))
listener.listen()

def stop(_signum, _frame):
    time.sleep(0.5)
    listener.close()
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
while True:
    connection, _ = listener.accept()
    connection.close()
PY
SERVER_PID=$!

for _ in $(seq 1 100); do
  worker_e2e_port_is_open "$PORT" && break
  sleep 0.05
done
worker_e2e_port_is_open "$PORT"

# Reproduce the old lifecycle's job-table removal and require the replacement
# barrier to work independently of it.
disown "$SERVER_PID" 2>/dev/null || true
worker_e2e_stop_process_and_port "$SERVER_PID" "$PORT"

if kill -0 "$SERVER_PID" 2>/dev/null; then
  echo "stopped listener process is still alive" >&2
  exit 1
fi

# Prove that a replacement can bind immediately, which is the contract needed
# by start_mock between Workerd phases.
python3 - "$PORT" <<'PY'
import socket
import sys

listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", int(sys.argv[1])))
listener.close()
PY

SERVER_PID=""
printf 'PASS Workerd mock restart waits for a disowned process and listener\n'
