#!/usr/bin/env bash
# Process helpers shared by the Workerd E2E harness and its lifecycle regression.

worker_e2e_port_is_open() {
  python3 - "$1" <<'PY'
import socket
import sys

probe = socket.socket()
probe.settimeout(0.1)
try:
    connected = probe.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0
finally:
    probe.close()
raise SystemExit(0 if connected else 1)
PY
}

worker_e2e_stop_process_and_port() {
  local pid="$1" port="$2" attempt

  kill "$pid" 2>/dev/null || true
  for attempt in $(seq 1 100); do
    # Treat `wait` as cleanup, not as the barrier: it is only safe to call once
    # both independent observations say the old server has stopped.
    if ! worker_e2e_port_is_open "$port" && ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" 2>/dev/null || true
      return 0
    fi
    sleep 0.05
  done

  echo "process $pid or listener on port $port did not stop" >&2
  return 1
}
