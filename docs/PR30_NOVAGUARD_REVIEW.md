# Archived v2.0.0 Nova Guard release validation

> Historical record — not a current runbook.
>
> This file preserves the final evidence for gateway pull request
> [#30](https://github.com/Noveum/ai-gateway/pull/30). The pull request is merged,
> v2.0.0 is published, and the earlier pre-merge “pending” report has been
> retired. Use [Validation](VALIDATION.md) for a new candidate and
> [Cloudflare Worker operations](CLOUDFLARE_WORKER.md) for current deployment
> instructions.

## Final release identity

| Artifact | Final value |
|---|---|
| Gateway pull request | [Noveum/ai-gateway #30](https://github.com/Noveum/ai-gateway/pull/30), merged |
| Gateway merge commit / v2.0.0 tag | `8a631c93a45f300950ac80c9b7230ce6debff3ff` |
| Companion control-plane pull request | Private Noveum app PR #544, merged and deployed |
| Companion merge commit | `eb866ffcab04edf11a4947e4a55339dcb5031b2f` |
| Crate | [noveum-ai-gateway 2.0.0](https://crates.io/crates/noveum-ai-gateway/2.0.0) |
| Rust documentation | [docs.rs 2.0.0](https://docs.rs/noveum-ai-gateway/2.0.0/noveum_ai_gateway/) |
| GitHub release | [v2.0.0](https://github.com/Noveum/ai-gateway/releases/tag/v2.0.0) |

## What was released

- Platform-managed Nova Guard on native and Cloudflare runtimes: effective
  policy fetch, live project/organization state, atomic strict admission, and
  complete/cancel/abandon reservation settlement.
- Native dedicated and shared tenancy. Shared mode derives tenant identity from
  each caller's scoped Noveum credential and refuses unentitled routing headers.
- Organization policies evaluate organization counters; project state is never
  substituted when organization state is absent.
- A versioned policy schema and named rejections for malformed, unknown,
  reserved-but-unenforceable, and invalid-source policies.
- Strict cost-cap bounds, provider-aware request preflight, conservative unknown
  pricing, authoritative usage settlement, and a versioned pricing catalog.
- Worker cache/refresh ordering, lease-safe timeouts, Anthropic buffered/SSE
  translation, and the 13-phase hermetic workerd regression suite.

The release is SemVer-major because `AppState::new`, `Policy::policy_type`, and
the error return of asynchronous `PolicyEngine::from_env` changed. See the
[migration guide](MIGRATING_TO_V2.md).

## Final automated evidence

The exact release candidate passed:

| Gate | Result |
|---|---|
| Rust library tests | 499 passed, 0 failed |
| Platform integration | 61 passed, 0 failed |
| Policy integration | 12 passed, 0 failed |
| Provider harness helpers | 3 passed, 0 failed |
| Formatting and strict Clippy | passed |
| Rustdoc with warnings denied | passed |
| Locked Cargo package/publish dry-run | passed |
| wasm32 check and pinned `worker-build 0.8.5` | passed |
| Wrangler `4.120.0 deploy --dry-run` | passed |
| 13-phase workerd bridge suite | passed |
| Docker and supply-chain CI | passed |

All 42 actionable review threads were resolved before merge. The final Worker
control-plane request path sends a stable versioned `User-Agent`; this was
required because the production CloudFront managed WAF correctly rejected
missing-user-agent subrequests. The WAF was not weakened.

## Control-plane and organization-scope proof

After companion PR #544 deployed, production probes created temporary
shadow/strict policies and two projects in one organization. A request in each
project incremented separate project counters while both reads returned the
same aggregate nested organization counter. Cancelling both reservations
restored project and organization state, and the temporary policy was deleted.

A separate `1d_calendar` probe admitted and accounted a bounded call, then
cancelled it and restored both scopes to zero. Policy model scoping was
confirmed as applicability filtering, not as a second spend ledger.

## Real runtime proof

### Native gateway

With real provider credentials supplied from an unprinted environment file,
OpenAI, Anthropic, and Groq each passed one buffered and one streaming call:
six calls, six HTTP successes. The runtime startup banner and `/health`
reported v2.0.0.

### Cloudflare

Dedicated guarded preview versions exercised the real Worker runtime against
the production Noveum control plane without changing production traffic. The
tests covered policy fetch, state, strict admission, provider dispatch,
settlement, two-project organization aggregation, and cleanup.

Production was then deployed to `https://gate.noveum.ai` and the service's
`workers.dev` hostname in **transparent** mode. This was intentional: a
project-bound dedicated bridge on a hostname shared by callers would attribute
all traffic to one project. Both domains reported healthy v2.0.0, real OpenAI,
Anthropic, and Groq buffered/SSE probes passed, and the observation window had
no Worker errors. At the 2026-08-24 verification point, Cloudflare routed 100%
of production traffic to version v57 and version v49 was retained as the
recorded rollback target.

As post-release cleanup, the authorized preview versions v50–v56 were deleted.
Follow-up requests to their version-prefix and alias hostnames returned 404,
and Cloudflare's GraphQL analytics showed no executions for the deleted
versions during the verification window. Version-prefix and alias preview URLs
are now disabled in `wrangler.toml` so a future upload does not silently restore
that exposure.

## Scope not claimed

- No v2.0.0 release claim was made for a paid live Fireworks, Together, or AWS
  Bedrock request. Their routing/conversion paths had hermetic coverage; live
  execution requires an available key, Region/model entitlement, and a bounded
  cost authorization.
- Streaming output content was not subject to output-phase Nova Guard
  enforcement. Input policies and terminal usage settlement still ran.
- The production shared Cloudflare hostname did not enable platform-managed
  guardrails. Guarded Cloudflare tenancy remained dedicated-only.
- Worker-native persistent telemetry and Workers KV policy loading were not
  part of v2.0.0.

These are explicit product boundaries, not hidden passes. Re-evaluate them for
each later release using [Validation](VALIDATION.md).
