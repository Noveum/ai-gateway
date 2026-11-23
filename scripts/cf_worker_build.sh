#!/usr/bin/env bash
# Builds the Cloudflare Worker bundle (build/index.js + build/index_bg.wasm).
#
# Single entry point for every place the Worker is built:
#   * wrangler.toml `[build] command`, so `wrangler dev` / `wrangler deploy`
#     run it;
#   * `npm run build`, which is the build command of the Workers Builds trigger
#     that deploys `main` to production.
#
# The Workers Builds image has no Rust toolchain, so under CI (CI or WORKERS_CI
# set) this installs rustup, the wasm32 target and the pinned worker-build when
# they are missing. On a developer machine it never installs anything: it
# fails with instructions instead.
#
# Workers Builds runs the build and deploy commands as separate shells, so the
# PATH set by a rustup install does not carry over; sourcing ~/.cargo/env below
# is what lets the `wrangler deploy` step find cargo again.
set -euo pipefail

# Same pin as WORKER_BUILD_VERSION in .github/workflows/rust.yml.
WORKER_BUILD_VERSION="${WORKER_BUILD_VERSION:-0.8.5}"
in_ci=false
if [[ -n "${CI:-}" || -n "${WORKERS_CI:-}" ]]; then
  in_ci=true
fi

if [[ -f "${HOME}/.cargo/env" ]]; then
  # shellcheck disable=SC1091
  . "${HOME}/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
  if [[ "${in_ci}" != true ]]; then
    echo "cargo not found. Install Rust (https://rustup.rs), then: rustup target add wasm32-unknown-unknown" >&2
    exit 1
  fi
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  . "${HOME}/.cargo/env"
fi

if command -v rustup >/dev/null 2>&1; then
  rustup target add wasm32-unknown-unknown
fi

installed_worker_build="$(worker-build --version 2>/dev/null || true)"
if [[ "${installed_worker_build}" != "${WORKER_BUILD_VERSION}" ]]; then
  if [[ "${in_ci}" != true ]]; then
    echo "worker-build ${WORKER_BUILD_VERSION} required (found: ${installed_worker_build:-none})." >&2
    echo "Install it with: cargo install worker-build --version ${WORKER_BUILD_VERSION} --locked" >&2
    exit 1
  fi
  cargo install worker-build --version "${WORKER_BUILD_VERSION}" --locked
fi

# Plain `--release`: worker-build keeps panic recovery on this path. See the
# `strip` comment in Cargo.toml before changing it.
exec worker-build --release
