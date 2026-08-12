# NovaGuard PR #30 — handoff

Continuation of the work described in the review doc
`https://orbit.noveum.ai/docs/ed553920-1537-4095-92d9-0ab57c4a2a32`.
Sections 4, 6.1, 6.2, 6.3 and 6.4 are implemented. This file records what
landed, what is deliberately unfinished, and what a follow-up session must
verify before any of it is enabled in production.

## Repos and branches

| Repo | Path | Branch | Base |
|---|---|---|---|
| Gateway (Rust) | `noveum/ai-gateway` | `feat/novaguard-platform-bridge` | `6826dc6` |
| Platform (TS) | `noveum/noveum-app-nextjs` | `feat/novaguard-composite-state` | `0a1ac4d5a` |
| Platform UI | worktree `noveum/noveum-app-ui` | `feat/novaguard-ui` | one commit `564c75bd1` |

## Verification gates (all green at handoff)

Gateway (`export PATH="$HOME/.cargo/bin:$PATH"`):

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --target wasm32-unknown-unknown --lib --all-features -- -D warnings
cargo test --lib               # 392  (was 236)
cargo test --test novaguard_platform   # 51  (was 18)
cargo test --test policy_integration   # 12  (was 10)
python3 codegen/generate_policy_types.py --check
python3 scripts/gen_pricing.py --platform-repo ../noveum-app-nextjs --check
worker-build --release
npx wrangler@4.120.0 deploy --dry-run          # needs Node >= 22
```

455 gateway tests, up from 264.

Platform (`export PATH="$HOME/.nvm/versions/node/v24.14.1/bin:$PATH"`):

```
cd packages/api && npx vitest run     # 672 passed | 2 skipped  (was 590)
cd apps/web    && bun run test        # 179 passed  (UI branch)
npx biome check <changed files>
```

Local infra: Postgres `127.0.0.1:5430` (`admin`/`admin@123`, db `noveum-db`)
and Redis `127.0.0.1:6379`, both from
`../scripts/docker-compose.noveum.yml`. The local Postgres is **prod-synced
with real data** — additive test rows only, never mutate existing ones.

## What is NOT done

### 1. NOV-132 unwrap audit — not started

~175 `.unwrap()` in `src/`. The crate builds with `panic = "abort"`, so a
failing unwrap on a request path kills the process and drops every in-flight
request on that replica. Deliberately scheduled last because it touches every
file and would have conflicted with all the parallel work; it is now unblocked
because the tree is quiet.

Catalogue the unwraps reachable from a request path, replace with explicit
error handling or justify as unreachable, and add `clippy::unwrap_used` scoped
to request-path modules so new ones cannot appear. Note `src/main.rs:207`
(`.expect("Failed to bind address")`) is startup, not request path — that one
is fine.

### 2. Section 6.4 documentation — implementation landed, docs did not

`README.md`, `k8/ai-gateway.yaml` and `docs/NOVA_GUARD.md` still describe
dedicated-project-only scope. They are accurate for dedicated mode and silent
about shared mode. **Do not enable shared mode on `gateway.noveum.ai` on the
strength of commit `9910c36` alone** — document the two modes, which one the
shared public deployment requires, and the key permissions
(`guardrails:read`, `guardrails:ingest`) first. The agent was terminated by a
session limit before this pass.

### 3. Worker bridge is unproven against a live edge

`worker_remote.rs` has 21 native tests covering every decision, and
`worker-build --release` plus `wrangler deploy --dry-run` both pass. There are
**no** `wasm-bindgen-test` / `worker` integration tests. The `worker::Fetch`
plumbing, `ctx.wait_until` behaviour and the stream tee under a real `workerd`
are proven only by compilation. Before shipping: `wrangler dev`, drive a real
streamed completion through it, and confirm a reservation actually settles.

### 4. Duplication to unify

`classify_admit`, the settlement request bodies and the credential matrix are
duplicated between `src/policy/admission.rs` (native, `reqwest`) and
`src/policy/worker_remote.rs` (wasm, `worker::Fetch`), because both native
modules are `reqwest`-bound at module level. Hoist the pure halves into a
shared wasm-safe module alongside `src/policy/metering.rs`.

### 5. Prisma `PolicyType` migration — written, not applied

`packages/database/prisma/POLICY_TYPE_MIGRATION.md`. The enum holds only
`COST_CAP` and `RATE_LIMIT`; the other 12 contract types need an additive
`ALTER TYPE ... ADD VALUE` affecting `ProjectPolicy.type` and
`GuardrailUsageEvent.blockedBy`, not reversible in place. Until then the API
returns a clean 400 naming the migration, and `PERSISTABLE_POLICY_TYPES` is
asserted equal to the Prisma enum in both directions so the gate cannot rot.
**`prisma db push` was deliberately not run against the prod-synced local DB.**

### 6. Phase 5 UI — items 1 to 5 done, the rest blocked

Done on `feat/novaguard-ui`: policy CRUD with per-type editors behind a
descriptor registry (plus `mode` and `priority`, which the platform already
stored but the UI could not reach), live usage with headroom bars reading the
nested `org` block, a violations feed on a new `GET /policies/violations`,
the effective-policy precedence view, and a dry-run panel that reuses the same
check-building as admission and reserves nothing.

- **NOV-105** (seven content rules) is one `PolicyTypeDescriptor` + form-schema
  member + fields component + i18n each, once the Prisma migration above lands.
  Three things do not fit the current shape and need decisions: `buildChecks`
  assumes a numeric counter vs a numeric limit and text rules have none; text
  rules carry `phases: input|output|both` and actions beyond `BLOCK`
  (redact/mask/flag) which the UI has no concept of; and the wire form is
  snake_case while the UI is on SCREAMING_SNAKE.
- **NOV-113** versioning, rollback and bundles, and **NOV-115** the SDK
  surface, have **no backend at all**. There is no policy-revision or bundle
  Prisma model, and `policyVersion` is only an ephemeral SHA-1 ETag of the
  effective set held in a Redis reservation. Schema work is required before any
  UI.
- The UI was never seen rendered while authenticated. The route is behind a
  session; the only ways in were creating an account or minting a session for a
  real person in the prod-synced dev DB, and both were declined. To look:
  `cd ../noveum-app-ui && cp ../noveum-app-nextjs/.env.local . && bun scripts/with-env.ts bun run --filter @repo/web dev -- --port 3005`

### 7. Open product decisions

- **Assumed price for an unknown model** is the catalog maximum, $15/$60 per
  1M (o1), derived not invented. Groq streaming placeholders (`unknown`,
  `llama`) now meter at that rate, far above the real llama rate. The better
  fix is making those placeholders resolvable rather than weakening the
  assumption. Orbit marks this `blocked_by` NOV-135.
- **Reject vs price-high** for unknown models. Today: allowed and priced high.
- **`pricingVersion` on reservation records** is half-done. Usage records carry
  it; `AdmitRequest`/`SettlementUsage` do not. `pricing::CATALOG_VERSION` and
  `reserve_request_breakdown(...)` are ready — it needs one field on
  `AdmitRequest`, one line in its `to_json`, and one at the construction site
  in `middleware.rs`.

### 8. Deployment order is mandatory

The platform composite-state change must deploy **before** the gateway drops
the org fallback. Reversed, expected fail-closed org policies block during
rollout.

Production `/state` had no `org` block at review time, and there is a real
org-scoped cap in org `5BzPKJ2FeldNjn4OQhlF1sZfvowF7NMW` ("cost cap 1",
`$1200/7d_rolling`, `failClosed: false`, policy `cmsivfwxt007o0ffs3c8v9h8i`)
that applies to every project and is **not enforced** until the platform side
ships.

## Bugs found by running the system, not by reading it

Recorded because each one was invisible to the test suite at the time.

1. **Cold-start cap breach.** Seeding used the readiness sentinel as its own
   lock, so losers of `SET NX` proceeded before history loaded. 30 concurrent
   $1 admissions against a $5 cap holding $0.40 settled at **$5.40**. Fixed;
   the regression test was mutation-checked (reverting the fix fails it 5-vs-4).
2. **Double metering.** Strict-mode requests were metered twice — once by
   settling the reservation, once by the legacy exporter — so every cost cap
   effectively halved. Fixed by making the settled reservation authoritative.
3. **Streams billed at the estimate.** Streams settled via `abandon`, retaining
   `input + max_output_tokens`. With `max_tokens` 4096 against a reply of tens
   of tokens that is ~100x over-billing. Fixed by teeing the stream and
   completing with real usage.
4. **Redis outage hung the API.** `maxRetriesPerRequest: null` queues commands
   forever; `/admit` hung for 30s instead of returning 503. Now 503 in ~21ms.
   Note the first fix attempt (disabling the offline queue outright) broke the
   first command under `lazyConnect` and failed 16 tests — it is gated on
   `ready` for that reason.
5. **Cancelled Sonnet 5 price increase.** The catalog scheduled $2/$10 to
   $3/$15 on 2026-09-01; Anthropic cancelled it. Would have overcharged 50%
   from that date **with no deploy**, while tests stayed green because they
   asserted the increase.
6. **`enforcementMode` stripped by Zod.** The platform's policy object had no
   such key, so Zod dropped it before storage — strict-mode cost caps could
   never be persisted, silently disabling the distributed enforcement path.
7. **Gateway deleted policy types on ingest.** `platform.rs` filtered the
   inbound feed to two types and `continue`d past the rest, with no log line.
   Its own unit test asserted the deletion.
8. **`worker-build --release`.** Not a wasm-bindgen/Rust incompatibility, as
   assumed. `[profile.release] strip = true` dropped the `target_features`
   custom section, so wasm-bindgen never saw reference-types.
   `-C target-feature=-reference-types` makes it strictly worse.

## End-to-end, with real providers

Verified against the local platform and real OpenAI + Anthropic keys: policy
CRUD, `/state` with the nested `org` block, admit → complete reconciliation,
idempotent replay, cap enforcement under a concurrent burst, and Anthropic
streaming emitting OpenAI-shaped SSE. A fail-closed cap correctly **blocked**
`claude-3-5-haiku` as unpriceable rather than metering $0.

Reproduce: `.claude/launch.json` has `ai-gateway`, `ai-gateway-worker` and an
attach-only `noveum-platform` entry. Gateway env needs `NOVEUM_API_URL`,
`NOVEUM_API_KEY`, `NOVEUM_GUARD_PROJECT_ID`, `NOVEUM_GUARD_ENABLED=true`,
`NOVEUM_GUARD_COST_ENFORCEMENT=strict`, and `PORT`.

Local test tenant (created additively): org `ngE2eOrg`, project `ngE2eProj`,
API key row `ngE2eKey` with an owner-role grant. Its key value is in the
session scratchpad, not in either repo.

**Rotate the production Noveum key that was pasted into the working session.**
It was not used or committed — a local key was generated instead — but it is in
a transcript.
