# Validation checklist

Use this checklist for a release candidate, a configuration change, or a
production deployment. Record the commit, artifact digest/version, runtime
configuration mode, test time, and result. “CI passed” is not evidence that a
specific production deployment is healthy.

## 1. Exact source and dependency graph

```bash
git status --short
git rev-parse HEAD
cargo metadata --locked --format-version 1 >/dev/null
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
git diff --check
```

The tree must be clean for a release artifact. Confirm the resulting SHA is the
reviewed SHA and contains no credentials or local environment files.

## 2. Hermetic native and policy tests

```bash
cargo test --locked --lib
cargo test --locked --bin noveum-ai-gateway runtime_uses_the_configured_worker_thread_count
cargo test --locked --test novaguard_platform
cargo test --locked --test policy_integration
cargo test --locked --test run_integration_tests streaming_smoke_
cargo test --locked --test run_integration_tests request_body_can_select_
cargo test --locked --test run_integration_tests model_override_uses_
```

These checks require no paid provider credentials. The three provider-harness
filters select seven network-free regressions: five streaming helpers, one
request-body helper, and one model-override helper. Together they cover the
policy engine, platform wire contract, routing helpers, and provider-aware
terminal-usage behavior without making a live call.

## 3. Cloudflare Worker and workerd

Use the pinned toolchain documented in
[Cloudflare Worker operations](CLOUDFLARE_WORKER.md):

```bash
rustup target add wasm32-unknown-unknown
cargo check --locked --target wasm32-unknown-unknown --no-default-features --lib
worker-build --release
npx --yes wrangler@4.120.0 deploy --dry-run
scripts/novaguard_worker_e2e.sh
```

The 13-phase workerd suite is the hermetic proof for Worker-only behavior such
as `worker::Fetch`, `ctx.wait_until` settlement, streaming tees, refresh
ordering, lease safety, bodyless `HEAD /` parity, cross-provider credential
stripping, and the complete Worker Bedrock unsupported-feature error envelope.
It does not replace a real edge smoke test.

## 4. Package and container

```bash
cargo build --locked --release
cargo package --locked --list
cargo publish --locked --dry-run
bash scripts/validate_docker_release.sh workflows
bash scripts/validate_docker_release.sh context
NOVEUM_VALIDATION_REVISION="$(git rev-parse HEAD)"
docker build --pull \
  --build-arg VERSION=2.0.1 \
  --build-arg REVISION="$NOVEUM_VALIDATION_REVISION" \
  --tag noveum-ai-gateway:candidate .
bash scripts/validate_docker_release.sh runtime-image \
  noveum-ai-gateway:candidate "$NOVEUM_VALIDATION_REVISION"
```

The runtime validator starts the image on a dynamic localhost port with a
read-only root filesystem, all capabilities dropped, and
`no-new-privileges`. It proves the configured and effective identity is exactly
`65532:65532`, the OCI version and source revision match the candidate, and
`/health` reports 2.0.1. The image sets `HOST=0.0.0.0` so Docker port publishing
and orchestrator probes can reach it; a native binary outside a container still
defaults to loopback. Inspect `cargo package --list` for accidental internal
reports, secrets, logs, and generated artifacts. A Cargo dry-run does not
validate Docker or Cloudflare.

The release-mode self-test proves that PR, `main`, and manual workflow events
are build-only, only the exact `v${Cargo package version}` tag publishes, and a
mismatched or non-mainline `v*` tag fails. The same wiring check proves that
credential-bearing provider smoke is job-gated to `refs/heads/main` before
checkout or secret exposure and checks out the exact event SHA. The context
check proves that the deny-by-default
`.dockerignore` admits only `Cargo.toml`, `Cargo.lock`, `src/`, `schema/`, and
`pricing/`, while representative `.env`, Wrangler-state, and unlisted files
cannot be copied. Release tags publish the version and `latest` to GHCR and
Docker Hub; production still pins the verified version digest.

Confirm the immutable image identifies the release you built:

```bash
docker image inspect noveum-ai-gateway:candidate \
  --format '{{ index .Config.Labels "org.opencontainers.image.version" }} {{ index .Config.Labels "org.opencontainers.image.revision" }} {{ .Config.User }}'
```

## 5. Live provider matrix

Use a dedicated low-budget test account, low output limits, and non-sensitive
prompts. Never print or commit keys. At minimum test:

| Provider path | Buffered | Streaming | Additional check |
|---|:---:|:---:|---|
| OpenAI | required | required | terminal usage and `[DONE]` |
| Anthropic | required | required | Messages conversion, terminal usage, optional function tool stream |
| Groq | required | required | response headers and provider/model identity |
| Together | when a test key is available | when a test key is available | current serverless model ID and terminal usage |
| Fireworks | when a test key is available | when a test key is available | exact account-qualified model ID and terminal usage |
| Gemini | when a test key is available | when a test key is available | OpenAI-compatible route and billed-output usage |
| OpenRouter | when a test key is available | when a test key is available | resolved model identity and conservative pricing boundary |
| Bedrock | when credentials/model entitlement are safely available | native only; the v2.0.1 Worker must return 400 `unsupported_feature` for `stream: true` | correct Region/model, SigV4, optional STS session token on both runtimes, and no credential logging |

For any unavailable key, model, Region, or account entitlement, record “not
tested” rather than treating absence as a pass. Provider credentials
authenticate the upstream; a Noveum key cannot replace them.

The checked-in smoke harness requests terminal usage explicitly only where that
field is part of the reviewed contract (OpenAI and Groq). Together and
Fireworks must emit terminal usage in their final provider chunks without an
undocumented `stream_options` option. A stream without authoritative usage is a
failed smoke, not a pass with incomplete accounting.

### Dated post-release evidence

On 2026-08-24, `https://gate.noveum.ai` passed buffered and streaming probes with
OpenAI `gpt-4o-mini`, Anthropic `claude-haiku-4-5-20251001`, Groq
`openai/gpt-oss-20b`, Together
`meta-llama/Llama-3.3-70B-Instruct-Turbo`, Fireworks
`accounts/fireworks/models/deepseek-v4-flash-0731`, Gemini
`gemini-2.5-flash-lite`, and OpenRouter `openrouter/free`. Direct AWS access to
`amazon.nova-micro-v1:0` passed buffered and streaming calls, but AWS credential
security prevented forwarding those credentials through the gateway; gateway
Bedrock remained unverified in that live audit. Azure OpenAI is not a routable
provider in v2.0.1. These results belong to that date, route, credential set,
and deployment—not to future provider availability.

## 6. Platform-managed Nova Guard

Use temporary shadow/strict policies and delete them after the test:

- [ ] Key scopes are exactly `guardrails:read`, `guardrails:ingest`, plus
  `projects:read` for a native shared caller.
- [ ] `/policies/effective` resolves and the expected policy source is
  `project` or `org`.
- [ ] Project policies read project counters; organization policies read the
  nested organization counters and never substitute project state.
- [ ] A bounded strict call is admitted once and completes once with actual
  input/output usage.
- [ ] An unbounded strict call returns 400 `missing_output_limit` before the
  provider is called.
- [ ] A gateway-side block cancels its reservation; an uncertain provider or
  stream outcome abandons rather than releasing the hold.
- [ ] Two projects in one organization see separate project counters and the
  same aggregate organization counter.
- [ ] No active test reservation or temporary policy remains after cleanup.

## 7. Production deployment

Before promotion:

- [ ] Record the current deployment/image/version as the rollback target.
- [ ] Confirm the candidate's bindings and secrets by **name**, without printing
  values.
- [ ] Confirm the tenancy choice. Never bind one dedicated project to a shared
  public hostname.
- [ ] Confirm persistent logging or a real-time tail is available.

After promotion:

- [ ] `GET /` returns the expected version/runtime page, and `HEAD /` returns
  the same status and security/cache headers with no body.
- [ ] Both the canonical domain and runtime-native domain return `/health` with
  the intended version.
- [ ] One low-cost buffered and one streaming call succeed through the exact
  production route.
- [ ] Authentication failures return the expected 401/403 and never reach the
  provider.
- [ ] Error rate, latency, and Worker/runtime exceptions remain normal through
  the observation window.
- [ ] Guarded deployments show expected admission and settlement; transparent
  deployments have no accidental Noveum bridge bindings.
- [ ] Rollback command/manifest is still valid.

If any mandatory check fails, stop promotion and restore the recorded artifact.
Do not “fix” a guarded outage by silently removing enforcement; use a deliberate,
audited emergency decision.
