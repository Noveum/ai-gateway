# Changelog

All notable changes to Noveum AI Gateway will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- GHCR release metadata now gives the immutable Cargo-version tag higher
  priority than `latest`, so `org.opencontainers.image.version` identifies the
  release instead of the moving alias. Unprivileged build validation now runs
  with `contents: read`; only a serialized, tag-only dependent release job has
  `packages: write` and Docker Hub secrets. Before either login or push, that
  job requires both exact version tags to return an authenticated anonymous
  `MANIFEST_UNKNOWN`; existing tags and every unauthorized, ambiguous, or
  network-failed response abort. After both pushes, automation uses an empty
  temporary Docker configuration to prove that the exact GHCR and Docker Hub
  tags are anonymously pullable, then reruns OCI, identity, hardening, and
  health validation on the pulled images.

### Documentation

- Recorded the immutable v2.0.1 GHCR caveat: its OCI version label is `latest`,
  and anonymous access failed at the 2026-08-24 release check. The Docker Hub
  v2.0.1 image has the correct version/revision labels. Existing v2.0.1 tags and
  images must not be overwritten; corrected GHCR metadata requires a later
  patch release.

## [2.0.1] - 2026-08-24

### Added

- Added a static, no-JavaScript landing page at `GET /` and `HEAD /` for native
  and Cloudflare runtimes. It reports the runtime and package version, links to
  health and public project resources, sends a restrictive content-security
  policy, and is cached for five minutes. `HEAD` preserves the response contract
  without a body. `/docs` and `/openapi.json` remain unimplemented.
- Native Bedrock signing now accepts the optional `x-aws-session-token` header,
  bringing temporary STS credential support to both native and Cloudflare
  runtimes.

### Fixed

- Cloudflare Worker Bedrock requests with `stream: true` now fail with a
  deterministic OpenAI-shaped HTTP 400 with
  `error.code = "unsupported_feature"` before Nova Guard admission or AWS
  dispatch. A present non-Boolean `stream` value is likewise rejected with a
  deterministic HTTP 400 instead of being treated as buffered mode. Native
  Bedrock ConverseStream remains supported; Worker Bedrock streaming is not
  implemented and is no longer allowed to enter an invalid signing/response
  path.
- The native server now honors the configured `HOST` when binding its TCP
  listener. The previous startup path logged `HOST` but always bound the
  wildcard address.
- `WORKER_THREADS` now determines the native Tokio runtime's worker count; the
  previous startup path applied the value only after Tokio had already created
  its runtime.
- The container image now sets `HOST=0.0.0.0` so published ports and
  orchestrator probes can reach the process, while native installations retain
  the loopback default. Release builds pass the Cargo version into the OCI
  image label instead of permanently labeling the image `latest`, record the
  exact source revision, and run as the fixed unprivileged UID/GID
  `65532:65532`.
- Container automation now publishes versioned GHCR and Docker Hub images (and
  advances `latest`) only for a pushed `v${Cargo package version}` tag. Pull
  requests, `main`, and manual runs build without publishing; a mismatched tag
  or a tag whose commit is outside trusted `main` fails before registry login.
  Every event validates the image with a read-only root filesystem, all
  capabilities dropped, `no-new-privileges`, the exact OCI labels, and a live
  versioned health response. The BuildKit context is deny-by-default and
  contains only the Cargo manifests, Rust source, policy schema, and pricing
  catalog.
- Cross-provider credential headers are stripped from generic upstream
  requests. `x-api-key` is reconstructed only for Anthropic, and `x-aws-*`
  headers are consumed only by Bedrock, so a credential for one provider is not
  leaked to another provider.
- Cloudflare version and alias preview URLs are explicitly disabled in
  `wrangler.toml`. This keeps preview hostnames dark after the deleted-version
  routing incident and makes future preview exposure an intentional,
  reviewable configuration change.
- Live-provider smoke defaults now use the model IDs verified on 2026-08-24,
  accept per-provider model overrides through repository/environment variables,
  can pass an optional AWS STS session token without storing it in source, and
  require authoritative terminal streaming usage. OpenAI and Groq request that
  usage explicitly; Together and Fireworks rely on their provider-emitted final
  chunks rather than an undocumented OpenAI option.
- Native Bedrock event-stream decoding now uses AWS's maintained Smithy parser,
  validates both frame checksums before translating a message to SSE, and
  propagates corrupt-frame failures to the downstream body. This removes the
  legacy parser chain that pulled in the unsound `lexical-core 0.7.6`.
- Native and integration-test environment loading now uses the maintained
  `dotenvy` fork instead of the unmaintained `dotenv 0.15.0`. Together with the
  Bedrock parser migration, the Rust security gates now carry no explicit
  advisory waivers.
- The integration harness no longer falls back to the repository's production
  `.env` when `.env.test` is absent. Hermetic CI now runs all eight network-free
  provider-helper regressions, including model override and provider-aware
  terminal-usage selection.
- The Workerd suite now verifies `HEAD /` parity, cross-provider credential
  stripping, the complete OpenAI-shaped Worker Bedrock `unsupported_feature`
  error envelope, and rejection of a non-Boolean Bedrock `stream` field rather
  than checking only a status or one field.

### Documentation

- Added a v1.2-to-v2 migration guide, a five-minute quick start, an explicit
  credential/scope matrix, production deployment and rollback runbooks, and a
  repeatable native/Worker/live-provider validation checklist.
- Made the packaged README's repository links resolve through the immutable
  `v2.0.1` Git tag instead of crates.io's moving `HEAD` rewrite.
- Preserved the v2.0.0 Cloudflare deployment as a dated release baseline and
  clarified, without pinning the current production version, that a shared
  hostname must remain transparent unless it is split into dedicated
  project-bound deployments.
- Replaced stale Fireworks and Together model inventories with dated,
  production-verified examples and documented the native/Worker Bedrock
  streaming split and AWS credential boundary.

## [2.0.0] - 2026-08-24

### Breaking changes

- The minimum supported Rust version (MSRV) is now **1.94.1** (previously
  1.91).
- `AppState::new` now accepts the live-state provider, usage reporter,
  admission client, and shared-tenancy resolver used by platform-managed Nova
  Guard. Downstream callers using the former three-argument constructor must
  pass the additional components (or `None`).
- `Policy::policy_type` is now `PolicyTypeTag` rather than `PolicyType`, so the
  gateway can preserve and report an invalid wire-level policy name. Use
  `Policy::kind()` to read its parsed `PolicyType` and `Policy::type_name()` to
  read the original string.
- `PolicyEngine::from_env` remains asynchronous but now returns `Result<Self,
  String>` so malformed or unsupported configuration fails explicitly instead
  of producing an apparently valid engine. Callers must continue to `.await`
  it and now handle the error.

### Added

- Platform-managed Nova Guard for the native gateway: effective-policy polling,
  live cost/rate state, atomic admission reservations, and complete/cancel/
  abandon settlement against the Noveum control plane.
- Organization-scoped guardrails backed by organization counters, including
  shared-key enforcement across multiple projects. Project policies continue to
  use project counters.
- Dedicated and shared native tenancy modes. Shared gateways authenticate each
  request and derive project/organization identity server-side rather than
  trusting caller-supplied tenant headers.
- The same platform bridge on Cloudflare Workers, with policy/config caching,
  strict admission, settlement, usage reporting, and a 13-phase Workerd
  end-to-end regression suite. Workers intentionally support dedicated tenancy;
  shared tenancy remains a native-gateway deployment mode.
- A versioned Nova Guard JSON Schema and checked-in policy-type registry.
  Unknown, malformed, and reserved-but-unenforceable policies now produce named
  rejections instead of disappearing silently.
- Provider-aware pricing from one checked-in catalog, including long-context
  tiers, cached-token rates, server-tool fees, provider/model aliases, a
  conservative unknown-model fallback, and a pricing/catalog version attached
  to reservations and usage.
- Authoritative buffered and streaming usage extraction, including Anthropic
  Messages-to-OpenAI SSE translation, so settlement uses provider-reported
  tokens and costs when available.
- Batched Nova Guard usage delivery with bounded queues, retry/backoff, event
  deduplication, and bounded shutdown flushing.
- Hermetic native, policy-platform, Worker/Workerd, supply-chain, and Docker CI
  gates, plus a separate real-provider smoke workflow.

### Changed

- Strict cost caps use platform-atomic admission across replicas; advisory caps
  continue to use cached live state. Rate-limit and cost-cap evaluation now
  distinguish unavailable state from a measured zero.
- Request estimates require explicit, bounded output-token limits when a strict
  cap needs them. Actual provider usage replaces the estimate at settlement;
  cancellations release holds and ambiguous disconnects abandon them safely.
- Model-scoped cost caps decide which requests a policy applies to while their
  spend ceiling is evaluated against the policy scope's aggregate spend.
- The pricing catalog includes current GPT-5.6 Sol promotional and long-context
  rates and preserves provider-specific pricing when model names overlap.
- Cloudflare Worker configuration changes no longer reuse a stale guard runtime,
  and panic-recovery metadata is retained in release WASM builds.

### Fixed

- Closed fail-open paths for unknown pricing, malformed policy bundles,
  unavailable admission/live state under fail-closed policies, and missing
  organization counters.
- Prevented duplicate metering and incorrect in-flight accounting across
  buffered, streaming, rejected, cancelled, and disconnected requests.
- Corrected reservation lifecycle handling so completed calls reconcile to
  actual usage, calls proven not to reach a provider cancel their hold, and
  uncertain outcomes do not release budget prematurely.
- Preserved upstream provider/model identity when accepting cost telemetry, so
  placeholder models cannot attach a mismatched price to a real model.
- Hardened tenant runtime caching against credential leakage and cache-pressure
  stampedes while keeping raw credentials out of cache keys and logs.

## [1.2.0] - 2026-06-26
### Added
- **One package, three deployment shapes.** The same `noveum-ai-gateway` crate
  now builds as the native binary / Docker image, a Rust library, **and a
  Cloudflare Worker** (`wasm32`, true per‑PoP edge). The Nova Guard engine,
  pricing, and request/response shaping are shared so all shapes behave
  identically. See [docs/CLOUDFLARE_WORKER.md](docs/CLOUDFLARE_WORKER.md) and
  [docs/CLOUDFLARE_DEPLOYMENT.md](docs/CLOUDFLARE_DEPLOYMENT.md).
- **All 13 providers on the edge Worker:** the OpenAI‑compatible set (OpenAI,
  Groq, Together, Fireworks, Mistral, Cohere, Gemini, DeepSeek, xAI, OpenRouter,
  Perplexity) plus **Anthropic** (`/v1/messages` + `x-api-key`, Messages→OpenAI
  conversion) and **Bedrock** (Converse API, **AWS SigV4** signed in pure Rust;
  also accepts **temporary credentials** via `x-aws-session-token`, which the
  native path does not).
- Worker Nova Guard parity: input block + redact, output‑phase block + redact,
  `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` (`synthetic_success`/`provider_error`),
  permissive CORS + `OPTIONS` preflight, header/query pass‑through, SSE
  streaming pass‑through, and an 8 MB response inspection cap.
- New shared modules `routing` (provider routing + request/response transforms)
  and `sigv4` (pure‑Rust AWS SigV4; unit‑tested against RFC 4231).

### Changed
- Native Anthropic now passes 4xx/5xx **error bodies through unchanged** instead
  of converting them to an empty `chat.completion`.
- Output‑phase enforcement picks the response shape from the actual body (so
  Anthropic responses already converted to OpenAI shape are still inspected).
- Output redaction now rewrites **every** choice / content block (was first‑only).
- `jsonschema` built without its HTTP/file `$ref` resolvers (wasm‑safe; no
  behavior change for inline schemas).

## [1.1.0] - 2026-06-25
### Added
- **Nova Guard** in-process policy enforcement, loaded from a **local** policy
  bundle (file or inline): model allow/deny, regex, banned substrings, PII &
  secrets detection, JSON-schema validation, and token caps — block, redact, or
  flag requests/responses. (`cost_cap`/`rate_limit` are present but require a
  cross-request state backend; without one they fail open.) See
  [docs/NOVA_GUARD.md](docs/NOVA_GUARD.md).
- New providers: Mistral, Cohere, Google Gemini, DeepSeek, xAI (Grok), OpenRouter,
  and Perplexity, served through a generic OpenAI-compatible adapter. See
  [docs/providers/openai-compatible.md](docs/providers/openai-compatible.md).
- Built-in, single-sourced model pricing table with per-request cost. See
  [docs/PRICING.md](docs/PRICING.md).
- Pluggable `MetricsExporter` trait with a console exporter (`DEBUG_METRICS`).

### Changed
- **Cost is now computed with dual (input/output) rates** from the shared pricing
  table for every provider, replacing per-provider single-rate estimates.
- Google and Cohere now route through their OpenAI-compatibility endpoints.
- Provider-name dispatch for metrics is case-insensitive.

### Removed
- **Elasticsearch exporter** and its configuration/dependencies.
- Unused configuration knobs and dead code; unused dependencies
  (`elasticsearch`, `tokio-retry`, `opentelemetry`, `metrics`, `backon`).

### Deferred (future work, not in this release)
- Noveum platform integration: trace export to the Noveum trace API, hosted
  Nova Guard policy distribution, and atomic budget reservation. The gateway runs
  self-contained with local policies; these will be added once the platform-side
  endpoints are available.

### Fixed
- `MAX_CONNECTIONS` is now honored (it was silently overridden by a hardcoded value).
- Fireworks responses now report cost (previously always missing).

## [1.0.1] - 2024-12-09
### Enhanced
- Improved ElasticSearch integration with more reliable data indexing
- Enhanced telemetry data formatting for better analytics
- Performance optimizations for high-volume request handling
- Code cleanup and formatting improvements throughout the codebase
- Documentation updates with clearer examples and instructions

### Fixed
- Connection handling issues with ElasticSearch during high load
- Memory leak in streaming response handling for long-running requests
- Inconsistent error reporting in provider-specific handlers
- Timeout issues with slow-responding upstream providers
- Race condition in concurrent request processing

## [1.0.0] - 2024-11-25
### Added
- Comprehensive telemetry system with robust metrics collection
- Elasticsearch integration for advanced log and metrics analysis
- Detailed request and response metrics tracking
- Integration tests for all supported providers
- Debug mode for local development and troubleshooting
- Plugin-based architecture for telemetry exporters (See [Telemetry Plugins Guide](docs/telemetry-plugins.md))
- OpenTelemetry compatible log format for standardized observability

### Enhanced
- Significantly improved logging system with structured logs
- Renamed project from MagicAPI to Noveum AI Gateway
- Optimized metrics middleware for performance monitoring
- Updated documentation to reflect new features and capabilities
- Console plugin for local metrics visualization
- Elasticsearch exporter for advanced analytics and visualization (removed in 1.1.0)
- Complete token usage tracking with cost estimation
- Detailed performance metrics for each request including latency and TTFB
- Provider-specific metrics for better monitoring and analysis
- Thread-based optimizations for improved concurrency handling

### Fixed
- Various performance bottlenecks in request processing
- Inconsistencies in provider metrics reporting
- Resource management for long-running requests

## [0.2.0] - 2024-11-20
### Added
- AWS Bedrock models support via the same gateway, allowing access to streaming capabilities through OpenAI-compatible interfaces.
- AWS request signing functionality in `src/proxy/signing.rs` for secure API requests.
- Comprehensive documentation for AWS Bedrock Provider integration in `docs/providers/bedrock.md`.

### Enhanced
- Updated provider documentation to reflect new features and improvements.
- Significant performance improvements across the codebase, including optimizations in `Cargo.toml`.

### Fixed
- Various minor bug fixes and improvements in request handling and processing.

## [0.1.7] - 2024-11-13
### Added
- Managed deployment offering with testing gateway at gateway.noveum.ai
- Thread-based performance optimizations for improved request handling
- Documentation for testing deployment environment
### Enhanced
- Significant performance improvements in request processing
- Build system optimizations
- CI/CD pipeline improvements
### Fixed
- Git build configuration issues
- Various minor bug fixes

## [0.1.6] - 2024-11-13
### Added
- Support for Fireworks AI provider
  - Complete integration with Fireworks API
  - Streaming and non-streaming support
  - Model-specific optimizations
- Support for Together.ai provider
  - Full API integration
  - Support for all Together.ai models
  - Streaming capabilities
### Enhanced
- Documentation updates for new providers
- Example usage for all supported providers
- Performance optimizations for streaming responses

## [0.1.5] - 2024-11-13
### Added
- Anthropic Claude support with automatic path transformation
- Provider framework restructuring
- Unified provider interface with trait-based implementation
### Enhanced
- Provider-specific path transformations
- Header processing across all providers
- Authentication flow standardization
### Fixed
- Header processing for streaming responses
- Error handling for invalid API keys
- Provider-specific status code handling

## [0.1.4] - 2024-11-07
### Added
- Docker support with multi-stage builds
- Docker Compose configuration
### Enhanced
- Documentation improvements
- Build process optimization

## [0.1.3] - 2024-11-07
### Added
- GROQ provider support
- Native integration with GROQ's ultra-fast LLM API
### Enhanced
- Stream handling improvements
- Error message clarity
- Request timeout handling
### Fixed
- Stream handling edge cases
- Memory management for long-running streams

## [0.1.0] - 2024-11-07
### Added
- Initial release
- Basic provider framework
- OpenAI support
- Streaming capabilities
- Error handling
- Basic documentation

[Unreleased]: https://github.com/Noveum/ai-gateway/compare/v2.0.1...main
[2.0.1]: https://github.com/Noveum/ai-gateway/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/Noveum/ai-gateway/compare/v1.2.0...v2.0.0
[1.2.0]: https://github.com/Noveum/ai-gateway/compare/65f678294a0609dcd1482bf2b1b9b830dcde42c5...v1.2.0
[1.1.0]: https://github.com/Noveum/ai-gateway/compare/v1.0.1...65f678294a0609dcd1482bf2b1b9b830dcde42c5
[1.0.1]: https://github.com/noveum/ai-gateway/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/noveum/ai-gateway/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/noveum/ai-gateway/compare/v0.1.7...v0.2.0
[0.1.7]: https://github.com/noveum/ai-gateway/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/noveum/ai-gateway/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/noveum/ai-gateway/compare/v0.1.4...v0.1.5
[0.1.3]: https://github.com/noveum/ai-gateway/compare/v0.1.0...v0.1.3
