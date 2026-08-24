#!/usr/bin/env python3

"""Hermetic behavior tests for fail-closed container tag preflight."""

from __future__ import annotations

import http.server
import json
import os
from pathlib import Path
import subprocess
import threading
import tomllib


ROOT = Path(__file__).resolve().parent.parent
VALIDATOR = ROOT / "scripts" / "validate_docker_release.sh"
with (ROOT / "Cargo.toml").open("rb") as cargo_manifest:
    VERSION = tomllib.load(cargo_manifest)["package"]["version"]
class RegistryHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        parts = self.path.split("?", 1)[0].strip("/").split("/")
        scenario = parts[0]

        if scenario == "network-failure":
            self.close_connection = True
            return

        if parts[-1] == "token":
            if scenario == "auth-denied":
                self._json(401, {"errors": [{"code": "UNAUTHORIZED"}]})
            else:
                self._json(200, {"token": "test-anonymous-token"})
            return

        if self.headers.get("Authorization") != "Bearer test-anonymous-token":
            self._json(401, {"errors": [{"code": "UNAUTHORIZED"}]})
            return

        registry = parts[1]
        if scenario == "existing-ghcr" and registry == "ghcr":
            self._json(200, {"schemaVersion": 2})
        elif scenario == "existing-dockerhub" and registry == "dockerhub":
            self._json(200, {"schemaVersion": 2})
        elif scenario == "ambiguous" and registry == "ghcr":
            self._json(404, {"errors": [{"code": "NAME_UNKNOWN"}]})
        else:
            self._json(404, {"errors": [{"code": "MANIFEST_UNKNOWN"}]})

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _json(self, status: int, payload: object) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def run_preflight(port: int, scenario: str) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.update(
        {
            "NOVEUM_RELEASE_GHCR_REGISTRY_BASE": f"http://127.0.0.1:{port}/{scenario}/ghcr",
            "NOVEUM_RELEASE_GHCR_TOKEN_URL": f"http://127.0.0.1:{port}/{scenario}/ghcr/token",
            "NOVEUM_RELEASE_DOCKERHUB_REGISTRY_BASE": f"http://127.0.0.1:{port}/{scenario}/dockerhub",
            "NOVEUM_RELEASE_DOCKERHUB_TOKEN_URL": f"http://127.0.0.1:{port}/{scenario}/dockerhub/token",
        }
    )
    return subprocess.run(
        [
            "bash",
            str(VALIDATOR),
            "release-tags-absent",
            f"ghcr.io/noveum/ai-gateway:{VERSION}",
            f"noveum/noveum-ai-gateway:{VERSION}",
        ],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )


def main() -> None:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RegistryHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        missing = run_preflight(server.server_port, "missing")
        assert missing.returncode == 0, missing.stderr

        for scenario, registry in (
            ("existing-ghcr", f"ghcr.io/noveum/ai-gateway:{VERSION}"),
            ("existing-dockerhub", f"noveum/noveum-ai-gateway:{VERSION}"),
        ):
            existing = run_preflight(server.server_port, scenario)
            assert existing.returncode != 0, scenario
            assert f"Release tag already exists: {registry}" in existing.stderr

        ambiguous = run_preflight(server.server_port, "ambiguous")
        assert ambiguous.returncode != 0
        assert "did not return MANIFEST_UNKNOWN" in ambiguous.stderr

        denied = run_preflight(server.server_port, "auth-denied")
        assert denied.returncode != 0
        assert "Anonymous registry token request failed" in denied.stderr

        network = run_preflight(server.server_port, "network-failure")
        assert network.returncode != 0
        assert "Anonymous registry token request failed" in network.stderr
    finally:
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()

    print(
        "Registry preflight accepts only MANIFEST_UNKNOWN and rejects existing, "
        "ambiguous, unauthorized, or network-failed tags"
    )


if __name__ == "__main__":
    main()
