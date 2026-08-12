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
cargo test --lib               # 395  (was 236)
cargo test --test novaguard_platform   # 51  (was 18)
cargo test --test policy_integration   # 12  (was 10)
python3 codegen/generate_policy_types.py --check
python3 scripts/gen_pricing.py --platform-repo ../noveum-app-nextjs --check
worker-build --release
npx wrangler@4.120.0 deploy --dry-run          # needs Node >= 22

# Live workerd, hermetic. NODE_BIN_DIR=... if node < 22 is first on PATH.
scripts/novaguard_worker_e2e.sh                # 18 assertions, 4 phases
```

458 gateway tests, up from 264, plus 18 live-edge assertions that no amount of
`cargo test` can replace.

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

### 1. NOV-132 unwrap audit — catalogued; fixes on a stacked branch

**Done:** `docs/NOV_132_UNWRAP_AUDIT.md` classifies every panicking call in
`src/` by reachability. The raw grep finds 331; 313 are inside `#[cfg(test)]`
modules, leaving **36 in production code** (9 `.unwrap()`, 26 `.expect()`, 1
`panic!`). Two structural findings retire most of the list: `panic = "abort"`
makes a `std::sync::Mutex` unpoisonable, and nearly every `Builder::body()` in
the tree is fed constant statuses and `&'static str` headers.

Three sites can fire on a request path, one reachable today — `remote.rs:38`
(`PLATFORM_CLIENT` is a `Lazy` that shared mode does not force at boot, so a
TLS-init failure aborts the replica on its first request, after readiness
passed), `proxy/client.rs:31`/`:49` (same shape), and `synthetic.rs:121` (the
only builder fed a platform-controlled header value).

**Not done:** the code changes. They are held out of PR #30 at the reviewer's
request and belong on `fix/nov-132-unwrap-audit`, which is stacked on this
branch — they touch `remote.rs` and `middleware.rs`, so branching from `main`
would conflict. The regression guard is
`#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` in
`lib.rs`; denying `expect_used` too is the point, since 26 of the 36 are
`.expect`. `src/main.rs` is a separate crate root and stays uncovered, which is
correct — startup is where an abort is the right behaviour.

### 2. Section 6.4 documentation — done (`3f1041e`)

`README.md`, `docs/NOVA_GUARD.md`, `k8/ai-gateway.yaml` and
`docs/CLOUDFLARE_WORKER.md` now document both modes side by side, which one the
shared public deployment needs, and the key permissions as a table
(`guardrails:read` + `guardrails:ingest` in both modes, `projects:read` in
shared, where there is no process-wide key at all).

Two stale claims were corrected while verifying the rest against the source:
both README and NOVA_GUARD.md still said a cross-replica guarantee "needs a
platform-side atomic reservation, which the API does not offer yet" — it does,
and strict mode uses it.

One real gap was found and fixed: `worker_rt.rs` never read
`NOVEUM_GUARD_TENANCY`, so `shared` on a Worker was **silently ignored** and
served a transparent proxy on a deployment its operator believed enforced
per-tenant caps. It now answers 503 `gateway_configuration_error`.

### 3. Worker bridge — proven against a live edge (`0e4b8e3`)

`scripts/novaguard_worker_e2e.sh` drives the Worker inside real `workerd` via
`wrangler dev`, hermetically (no Cloudflare account, no backend, no provider
key — the bundled mock plays both the control plane and the OpenAI upstream).
18 assertions across four phases, each on a fresh isolate.

Phase 1 is the one to read: a `gpt-4o` request with `max_tokens: 4096` reserves
`$0.040965` and settles at `$0.0000675`, because the tee recovered 11 in / 4 out
from the terminal usage frame — and the settlement lands *after* the body
completes, which is `ctx.wait_until` working. Phases 2 to 4 cover abandon-on-no-
usage, 503-is-never-an-allow, and the HTTP-200 block.

Two things were needed to make it testable: `ProviderRoute.base_url` was a
compile-time `&'static str`, so the Worker now honors `OPENAI_BASE_URL` like the
native side (sharing one normalization rule); and the mock gained `/admit`,
the three settlement endpoints and an SSE mode.

**Remaining gap:** `StreamOutcome::Dropped` is still unproven. Cutting the client
off mid-stream does not reach it, because `workerd` drains the upstream to EOF
anyway, so the tee still recovers authoritative usage and correctly completes.

### 4. Duplication — unified (`b2112fd`)

`src/policy/admission_wire.rs` is now the single definition of the admission
wire contract, re-exported by both clients. 791 lines deleted. `truncate_body`
had a third copy in `remote.rs`; that one re-exports too.

`WorkerRemoteConfig::from_values` deliberately did **not** move: it mirrors the
native `RemoteConfig::from_values` because the two must agree on what a
half-applied secret set *means*, not because they are the same code.

### 5. Prisma `PolicyType` migration — applied (`a0fe38491`)

All 14 contract types now persist. `PENDING_MIGRATION_POLICY_TYPES` emptied
itself, and the API no longer 400s on the other twelve.

Applied **without** `prisma db push`, which re-diffs the whole schema and can do
more than what you reviewed: `pg_dump` backup first (113M, `/tmp`), then
`prisma migrate diff --script` to produce the exact SQL, then a check that it was
nothing but `ALTER TYPE ... ADD VALUE`, then `psql`. 12 statements, no other
drift, and `project_policies` (10 rows) / `guardrail_usage_events` (100 rows)
byte-identical afterwards.

Two things the migration doc did not anticipate:

- One drift test was obsolete by construction — it asserted `SECRETS_DETECTION`
  gets a 400 naming the migration, which is the gap being closed. Replaced with
  its inverse.
- **Widening the enum broke `tsc`**; left alone the API package would not
  compile. Two call sites passed the 14-member union into code typed
  `"COST_CAP" | "RATE_LIMIT"`. Narrowed at both boundaries
  (`isAdmissionPolicyType`) rather than by widening admission, which the doc
  calls separate work. Not just a type fix: `buildAdmissionChecks` does
  `type === "COST_CAP" ? … : rateLimitChecks`, so an unfiltered `PII_DETECTION`
  policy would have fallen into the rate-limit arm, produced zero checks, and
  been reported to the caller as *unparseable*.

**NOV-105 is now unblocked** — it is one `PolicyTypeDescriptor` + form-schema
member + fields component + i18n per rule, plus the three shape mismatches in
§6 that still need a decision.

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

- ~~**Assumed price for an unknown model**~~ **DECIDED (`cb083ae`).** The rate
  stays the derived catalog maximum: it rises with the catalog and errs in the
  only safe direction. The Groq over-metering it was blamed for was a different
  bug — those streams carried a *placeholder* id (`llama`, `claude`) that no
  event ever populated, so a call whose model was never learned was priced as an
  unknown *model*. `resolve_model_for_metering` now resolves it from the request
  body, which is the same value admission already reserves against.
- ~~**Reject vs price-high**~~ **DECIDED (`cb083ae`): priced high, never
  rejected.** A pass-through proxy refusing an unrecognized model id turns every
  provider launch into an outage. Operators wanting an unmeterable call refused
  have a better per-policy lever: `failClosed: true` on a `cost_cap`.

  Deciding it surfaced a real hole, now closed: admission returned `None` for an
  unknown model and both call sites read that as $0, while billing charged the
  catalog maximum. A cost cap could be walked past by naming a model the gateway
  had never heard of. `pricing::reserve_request_cost` always yields, so
  reserving and billing rest on the same assumption.
- **`pricingVersion` on reservation records** — gateway side done (`8b4f7e4`).
  `AdmitRequest` and `SettlementUsage` carry it, and both native and Worker
  construction sites stamp `pricing::CATALOG_VERSION`; verified on the wire
  through `wrangler dev`, not just in tests.

  **It is inert until the platform accepts it**, and this is the same shape as
  the `enforcementMode` bug above. The platform's `admitRequestSchema` and
  `completeReservationSchema` are plain `z.object(...)`, so Zod **strips** the
  key at ingress, and there is no column for it — `GuardrailUsageEvent` has no
  `pricingVersion` field in `schema.prisma`. That also corrects an earlier note
  here: "usage records carry it" was true of the gateway's outbound payload
  only; `/usage` has been sending a field the platform discards. Landing this
  end to end needs a platform schema change plus an additive migration, and the
  migration is blocked on the same decision as `POLICY_TYPE_MIGRATION.md`.

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
