# Nova Guard — Policy Enforcement in the Gateway

Nova Guard is the policy-enforcement layer built into the Noveum AI Gateway. It
inspects every LLM request and response flowing through the gateway and can
**block**, **redact / mask**, or **flag** based on policies you define. It runs
the deterministic policy types entirely in-process (no network dependency), from
a **local policy bundle** (file or inline env) — the gateway enforces guardrails
standalone (BYOK mode) with no external service.

Policies can come from either of two places:

* a **local bundle** (file or inline env), enforced entirely in-process; or
* the **Noveum control plane** ("platform-managed Nova Guard"), which also
  supplies the live cost/rate counters that `cost_cap` and `rate_limit` need,
  and accepts usage reports back. See
  [Deployment modes](#deployment-modes-dedicated-and-shared).

## Quick start

1. Write a policy bundle (`nova-guard.json`) — see [Policy bundle](#policy-bundle).
   A ready-to-use starter lives at
   [`nova-guard.example.json`](../nova-guard.example.json) in the repo root.
2. Point the gateway at it:

   ```bash
   export NOVEUM_GUARD_ENABLED=true
   export NOVEUM_GUARD_POLICIES_FILE=./nova-guard.example.json
   cargo run --release
   ```

3. Send requests as usual (`x-provider: openai`, `POST /v1/chat/completions`).
   Requests that violate an `enforce`-mode policy are blocked before reaching the
   provider; `shadow`-mode policies are evaluated and logged but never block.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `NOVEUM_GUARD_ENABLED` | `true` | Master switch. `false`/`0` makes the gateway a pure pass-through. |
| `NOVEUM_GUARD_POLICIES_FILE` | _(unset)_ | Path to a `nova-guard.json` bundle. |
| `NOVEUM_GUARD_POLICIES` | _(unset)_ | Inline JSON bundle (used if no file is set). |
| `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` | `synthetic_success` | `synthetic_success` (HTTP 200 with a refusal completion) or `provider_error` (HTTP 403 with the provider's error envelope). |

When the engine is disabled or has zero active policies, the middleware
short-circuits without buffering the body, so guardrails add **no overhead** when
unused.

## Policy bundle

The bundle format (`nova-guard.json`) is shared with the Nova Guard SDK:

```json
{
  "bundleVersion": "1.0",
  "policies": [
    {
      "name": "Block US SSN in output",
      "type": "regex_match",
      "mode": "enforce",
      "priority": 100,
      "config": {
        "phase": "output",
        "patterns": [{ "name": "ssn_us", "regex": "\\b\\d{3}-\\d{2}-\\d{4}\\b" }],
        "action": "block"
      }
    },
    {
      "name": "Strip PII from prompts",
      "type": "pii_detection",
      "mode": "enforce",
      "config": {
        "phase": "input",
        "entities": ["EMAIL_ADDRESS", "PHONE_NUMBER", "US_SSN"],
        "action": "mask"
      }
    },
    {
      "name": "Monthly LLM budget",
      "type": "cost_cap",
      "mode": "enforce",
      "failClosed": true,
      "config": { "window": "1mo_calendar", "maxUsd": 1500, "action": "block" }
    }
  ]
}
```

### Common policy fields

| Field | Default | Meaning |
|---|---|---|
| `name` | _(required)_ | Unique policy name (used as the id if `policyId` is omitted). |
| `type` | _(required)_ | One of the policy types below. |
| `mode` | `shadow` | `off` / `shadow` / `enforce`. |
| `failClosed` | `false` | When live state is unavailable, block instead of allowing. |
| `priority` | `100` | Lower runs first; ties broken by name. |
| `enabled` | `true` | Set `false` to disable without removing. |
| `config` | `{}` | Type-specific configuration. |

## Policy types

The policy contract is one versioned JSON Schema,
[`schema/novaguard-policy.v1.json`](../schema/novaguard-policy.v1.json). It is the
single source of truth for the gateway, the Noveum platform and the Python SDK;
the Rust `PolicyType` enum and the platform's TypeScript module are both
generated from it by `codegen/generate_policy_types.py`, and CI fails on drift.
Add a policy type there, never by hand in a consumer.

Fourteen types are defined. The nine below are **enforced**. The five classifier
types (`scorer_gate`, `prompt_injection`, `topic_restriction`,
`content_moderation`, `grounding_check`) are **reserved**: their config is
validated, but enforcing them needs an external scoring service this build does
not call.

A policy the engine cannot enforce is never silently skipped. It is logged at
`error!` naming the policy and the reason, counted by
`PolicyEngine::rejected_policies()`, and refuses startup when it came from a
local bundle. If the policy is marked `failClosed`, it blocks traffic: a policy
that can never compile can never be evaluated, which is exactly what
`failClosed` is for. Shadow mode still never blocks.

| Type | Phase(s) | What it does |
|---|---|---|
| `cost_cap` | input | Block/flag when spend over a window crosses a cap (soft + hard). **Requires a cross-request state backend** — supplied by platform-managed Nova Guard on the native gateway; without one it fails open. See [Modes and fail-safety](#modes-and-fail-safety). |
| `rate_limit` | input | Block when requests/tokens over a window exceed a limit. **Requires a cross-request state backend** — same as `cost_cap`. |
| `model_allowlist` | input | Allow only listed models (or deny specific ones); supports `gpt-4*` wildcards. |
| `regex_match` | input/output/both | Block/redact/mask/flag on regex matches (linear-time, ReDoS-safe). |
| `banned_substrings` | input/output/both | Block/redact a fixed list of literal terms (Aho-Corasick). |
| `pii_detection` | input/output/both | Detect + mask/redact/block PII (email, SSN, credit card, phone, IP, IBAN, passport). |
| `secrets_detection` | input/output/both | Detect leaked credentials (AWS/GitHub/GitLab/Slack/Stripe/OpenAI/Anthropic/Google keys, JWT, PEM). |
| `json_schema` | output | Validate the model output is JSON conforming to a schema (optional Markdown-fence stripping). |
| `token_length_cap` | input | Block when token count exceeds a cap (per-model scope). |

### Actions

`block` · `redact` · `mask` · `hash` · `replace` · `flag_only`. Transform actions
(`redact`/`mask`/`hash`/`replace`) rewrite the matched spans; `flag_only` records
the decision without changing the payload. In `shadow` mode no action is applied
regardless of the configured action.

## Modes and fail-safety

* **off** — not evaluated.
* **shadow** — evaluated, decisions logged, payload untouched. Use this to
  observe a new policy before enforcing it.
* **enforce** — decisions applied: blocks short-circuit with a provider-shaped
  synthetic response; transforms rewrite the payload.

`block` is terminal: the first enforced block aborts the chain. Transforms
compose — each policy (in priority order) sees the previous policy's mutated
text.

`cost_cap` and `rate_limit` need a cross-request state backend (spend/rate
counters) to evaluate. There are two cases:

* **No backend** (local bundle only, or the Cloudflare Worker) — they cannot be
  evaluated, so they **always fail open** (allow), and a `failClosed: true` on
  those two types is neutralized at load time with a warning so a stale bundle
  can't block 100% of traffic. The Worker refuses such a bundle outright with a
  503 rather than accepting it and enforcing nothing.
* **Platform-managed Nova Guard** — live counters come from the Noveum platform,
  so both types evaluate for real and `failClosed` is honored: a `/state`
  outage, or a counter the policy needs that the response does not carry,
  blocks.

(The stateless policy types — regex, PII, secrets, banned substrings, model
allowlist, JSON schema, token caps — enforce fully with no backend.)

## Deployment modes: dedicated and shared

Platform-managed Nova Guard runs in one of two mutually exclusive modes,
selected by `NOVEUM_GUARD_TENANCY`. The question that decides it is whether the
deployment serves more than one tenant.

| Env var | Dedicated | Shared |
|---|---|---|
| `NOVEUM_GUARD_TENANCY` | `dedicated`, or unset | `shared` |
| `NOVEUM_API_KEY` | required | **must be unset** |
| `NOVEUM_GUARD_PROJECT_ID` | required | **must be unset** |
| `NOVEUM_API_URL` | optional | optional |
| `NOVEUM_GUARD_TENANT_TTL_SECS` | n/a | optional, default `300` |
| `NOVEUM_GUARD_TENANT_CACHE_MAX` | n/a | optional, default `1024` |
| `NOVEUM_GUARD_POLICIES[_FILE]` | allowed | **refused at startup** |

Configuring both modes at once aborts startup rather than resolving by
precedence, and shared mode is never inferred — it is only entered by asking
for it, so an existing dedicated deployment cannot drift into it.

**Dedicated** pins the whole process to one project. Every request a replica
handles is metered and capped against it regardless of who sent it, which is
correct for a gateway fronting one team and wrong for anything else. The policy
set is fetched at startup (a failed first fetch aborts) and refreshed by a
background poller.

**Shared** authenticates each caller and derives its tenant server-side:

* The caller presents its own Noveum key in **`x-noveum-api-key`** — not
  `Authorization`, which on this gateway carries the caller's *provider* key and
  is forwarded upstream. The tenancy layer strips `x-noveum-api-key` from the
  request before proxying, so a tenant's Noveum credential never reaches a model
  provider. A `Bearer ` prefix is accepted.
* `x-project-id` and `x-organization-id` are **routing filters, not identity**.
  A project header may only select among the projects the credential is already
  entitled to; naming another project is rejected rather than silently
  overridden. An organization header must match the derived organization. With
  no project header and exactly one entitlement, that project is used; with
  several, the request is refused rather than metered against a guess.
* Policies, live counters, reservations, usage records and every cache entry are
  keyed by the **derived** tenant. A cache keyed on client input would be a
  cross-tenant leak by construction.
* Every failure is a refusal, never a fallback to a default project: `401` for a
  missing or rejected credential, `403` for authenticated-but-not-entitled
  (answered identically whether the project belongs to another organization or
  does not exist, so it leaks nothing), `400` when the credential is entitled to
  several projects and the request named none, `503` when no verdict could be
  reached.
* Nothing is fetched at startup — there is no tenant yet. Each tenant's policy
  set is fetched on its first request, and a tenant idle for 10 minutes is
  dropped, which is also what lets a revoked credential self-heal.
* `/health` requires no credential, so probes are unaffected. Only `/v1/*`
  requires a tenant.

### Key permissions

| Permission | Needed for | Mode |
|---|---|---|
| `guardrails:read` | `/policies/effective` + `/policies/state` | both |
| `guardrails:ingest` | `/policies/usage` + reservation settlement | both |
| `projects:read` | deriving the caller's project + organization | shared only |

In dedicated mode this is the deployment's own scoped service key, set once via
`NOVEUM_API_KEY`. In shared mode there is no process-wide key at all: each
platform call is made with the calling tenant's own credential.

### Scope of enforcement under the platform bridge

* A policy reads **only its own scope's counters**. An organization-sourced
  policy is evaluated against organization counters; if the platform's `/state`
  response does not carry them, that is *unavailable state* (fail-closed blocks,
  fail-open allows with an explicit reason) — project counters are never
  substituted, because that would let every project consume the whole
  organization allowance separately.
* Admission is **estimate-based in both modes**, but only one of them holds
  across replicas. A `cost_cap` with `enforcementMode: strict` (or a deployment
  with `NOVEUM_GUARD_COST_ENFORCEMENT=strict`) reserves against the platform's
  atomic admission API, so every replica shares one counter. An **advisory** cap
  reserves only in the replica's own ledger, so the effective overshoot is
  multiplied by replica count. See the README for the full comparison.
* In **shared** mode this is per derived tenant: each tenant gets its own
  engine, counters, ledger and reservations, so one tenant's traffic can neither
  consume nor observe another's headroom.

## Streaming

Streaming responses (`text/event-stream`) are passed through without
output-phase enforcement in v1 (a limitation shared across LLM gateways).
Input-phase enforcement and blocking still apply to streaming requests.

## Extending Nova Guard

Adding a new deterministic policy type is local:

1. Add a variant to `PolicyType` (`src/policy/config.rs`) and a config struct.
2. Implement `PolicyRule` in `src/policy/rules/<your_rule>.rs`.
3. Add a `parse` arm in `compile_rule` (`src/policy/rules/mod.rs`).

The engine, middleware, telemetry, and synthetic-response layers pick it up with
no further changes.

## Cost / pricing

Per-request cost is computed from the model pricing table in
`src/policy/pricing.rs` (current as of June 2026; see the table's caveats on
Gemini's >200K tier, cache-hit pricing, and provider-prefixed model ids). The
table is single-sourced and reused by the provider metrics extractors.
