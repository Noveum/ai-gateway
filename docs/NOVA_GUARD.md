# Nova Guard — Policy Enforcement in the Gateway

Nova Guard is the policy-enforcement layer built into the Noveum AI Gateway. It
inspects JSON requests and buffered JSON responses and can **block**, **redact /
mask**, or **flag** based on policies you define. Streaming requests still run
input policies and usage metering, but streaming output content is not buffered
for output-phase enforcement in v2.0.x. It runs
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
| `NOVEUM_GUARD_ENABLED` | `true` | Master switch. A false value disables policy evaluation. A configured platform bridge may still initialize and report usage, so remove the bridge settings too when transparent mode is intended. |
| `NOVEUM_GUARD_POLICIES_FILE` | _(unset)_ | Path to a `nova-guard.json` bundle. |
| `NOVEUM_GUARD_POLICIES` | _(unset)_ | Inline JSON bundle (used if no file is set). |
| `NOVEUM_GUARD_BLOCK_RESPONSE_MODE` | `synthetic_success` | `synthetic_success` (HTTP 200 with a refusal completion) or `provider_error` (HTTP 403 with the provider's error envelope). |
| `NOVEUM_GUARD_COST_ENFORCEMENT` | policy setting | Native gateway only: `strict` forces cost caps through platform-atomic admission; `advisory` forces the per-process ledger; unset lets each cap's `enforcementMode` decide. `strict` requires the platform bridge. |
| `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` | `1024` | Fallback completion estimate for advisory cost caps and rate-only admission. It is not a substitute for the explicit output limit required by an applicable enforcing/blocking strict cost cap. |

When the engine is disabled or has zero active policies, the middleware
short-circuits without buffering the body or evaluating rules. Normal gateway
routing and telemetry still run.

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
      "name": "Bound prompt length",
      "type": "token_length_cap",
      "mode": "enforce",
      "config": { "phase": "input", "maxTokens": 100000, "action": "block" }
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
[`schema/novaguard-policy.v1.json`](../schema/novaguard-policy.v1.json), embedded
into the binary and used to validate every policy `config`. The Rust
`PolicyType` enum in `src/policy/policy_types.rs` and the Noveum platform's
TypeScript equivalent are maintained alongside it by hand; a new policy type has
to be added in each place.

Fourteen types are defined. The nine below are **enforced**. The five classifier
types (`scorer_gate`, `prompt_injection`, `topic_restriction`,
`content_moderation`, `grounding_check`) are **reserved**: their config is
validated, but enforcing them needs an external scoring service this build does
not call.

A policy the engine cannot enforce is never silently skipped. It is logged at
`error!` naming the policy and the reason, counted by
`PolicyEngine::rejected_policies()`, and refuses native startup when it came
from a local bundle. A Worker inline bundle is evaluated per proxy request, so
an invalid bundle returns a configuration error on `/v1/*` while `/` and
`/health` can remain available. If the policy is marked `failClosed`, it blocks
traffic: a policy that can never compile can never be evaluated, which is
exactly what `failClosed` is for. Shadow mode still never blocks.

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

* **No backend** (a local bundle on the native gateway, or an inline Worker
  bundle) — they cannot be evaluated, so the native engine **fails them open**
  (allow) and neutralizes `failClosed: true` at load time with a warning. The
  Worker refuses an inline bundle containing either stateful policy with a 503
  rather than accepting it and enforcing nothing.
* **Platform-managed Nova Guard** — live counters come from the Noveum platform,
  so both types evaluate for real on the native gateway **and** Cloudflare
  Worker. `failClosed` is honored: a `/state` outage, or a counter the policy
  needs that the response does not carry, blocks.

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
| `NOVEUM_GUARD_POLICIES[_FILE]` | not combined with the platform source; remove for clarity | **refused at startup** |

Configuring both modes at once aborts startup rather than resolving by
precedence, and shared mode is never inferred — it is only entered by asking
for it, so an existing dedicated deployment cannot drift into it.

The Cloudflare Worker implements dedicated mode only. Do not attach one
dedicated project to a hostname shared by unrelated callers: every call would
be attributed and capped against that project. Keep that Worker transparent,
deploy a dedicated Worker/domain per project, or use the native shared mode.

**Dedicated** pins the whole deployment to one project. Every request a replica
handles is metered and capped against it regardless of who sent it, which is
correct for a gateway fronting one team and wrong for anything else. The policy
set is fetched before guarded traffic is allowed. Native refreshes it with a
background poller; the Worker revalidates its per-isolate cache on requests. A
failed first fetch refuses startup/native traffic or the Worker request unless
the explicit emergency unguarded-start override is enabled.

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
  across replicas. A `cost_cap` with `enforcementMode: strict` (or, on the
  native gateway, a deployment with
  `NOVEUM_GUARD_COST_ENFORCEMENT=strict`) reserves against the platform's atomic
  admission API, so every replica shares one counter. An **advisory** cap on the
  native gateway reserves only in the replica's own ledger, so the effective
  overshoot is multiplied by replica count. The Worker still calls the platform
  admission bridge for stateful policies, but it does not promote an advisory,
  shadow, non-blocking, or out-of-scope cap into the strict output-bound
  contract.
* In **shared** mode this is per derived tenant: each tenant gets its own
  engine, counters, ledger and reservations, so one tenant's traffic can neither
  consume nor observe another's headroom.

### Strict caps and explicit output limits

A hard cost cap cannot safely reserve an unbounded provider response against a
heuristic. Before admission, the gateway therefore accepts these aliases:

1. `max_tokens`
2. `max_completion_tokens`
3. `max_output_tokens`

Every non-null alias must contain the same positive integer no greater than
10,000,000. Conflicting aliases receive HTTP 400 before admission. Transparent
OpenAI-compatible routes are rewritten to one `max_tokens` field equal to the
admitted bound; Anthropic and Bedrock converters map the same bound to their
native request field.

| Policy state for the requested model | Unbounded request |
|---|---|
| `mode: enforce`, `action: block`, effective strict mode, model in `scopeToModels` (or no scope) | **HTTP 400** with `error.code: "missing_output_limit"`; no platform reservation and no provider call |
| Advisory `cost_cap` | Allowed using `NOVEUM_GUARD_ASSUMED_OUTPUT_TOKENS` as the estimate; this is not a hard guarantee |
| `rate_limit` only | Allowed; rate-only policies do not require an output bound and use the fallback where token estimation needs one |
| Strict cap in `shadow` mode, disabled, or outside `scopeToModels` | Allowed; that cap is not an enforcing block for this request |

The scope rows assume a parseable JSON body with a trustworthy `model`. If any
enforcing/blocking strict cap is active, a multipart, binary, or otherwise
non-JSON body cannot prove that it is out of scope and is rejected with HTTP 400
before admission rather than silently bypassing the cap.

On the native gateway, `NOVEUM_GUARD_COST_ENFORCEMENT=strict` selects strict
handling deployment-wide, including for caps declared advisory;
`NOVEUM_GUARD_COST_ENFORCEMENT=advisory` selects advisory handling even for a
cap declared strict. The Cloudflare Worker uses each policy's
`enforcementMode`; it does not implement this native deployment override.

The strict input contract is deliberately narrower than transparent proxying.
It accepts bounded JSON `/v1/chat/completions` with a non-empty model, a
messages array, and one completion (`n`/`best_of` must be absent or `1`). It
estimates the complete post-transform serialized body and rejects Responses or
server-side conversation state, images, files, audio, remote search,
server/MCP tools, Perplexity, and OpenRouter before admission. Client function
tools remain supported on adapters that translate or transparently pass them;
the Bedrock adapter does not yet translate tool use. Strict Bedrock admission
accepts only catalogued commercial Claude direct/geographic/global model IDs
and identifiable system inference profiles. Region-priced Nova/Titan models
remain available under advisory policies but receive HTTP 400
`unsupported_strict_input` before reservation under a strict cost cap. Direct
OpenAI calls are pinned to `service_tier: "default"`; requested premium tiers
are rejected.
Advisory policies retain the broader pass-through surface.

The output ceiling is only one part of a strict reservation. Provider-specific
declarations that can raise the bill are included before admission. For
Anthropic, any prompt-cache breakpoint reserves the entire estimated prompt as
a first cache write at the longest declared TTL; `inference_geo: "us"` reserves
1.1x, and omitting geo on an eligible model also reserves 1.1x because the
workspace can select US-only inference. Opus 5/4.8 `speed: "fast"` reserves 2x;
fast plus US geo reserves 2.2x. Cache controls, geo, speed, mixed-model
`fallbacks`, constrained-model sampling, and Sonnet 5 thinking/prefill are
validated before a reservation exists. See the
[Anthropic contract](providers/anthropic.md).

## Streaming

Streaming responses (`text/event-stream`) skip output-phase enforcement in this
release; input-phase enforcement and blocking still apply. OpenAI-compatible
provider streams pass through incrementally. Successful Anthropic Messages
streams are translated incrementally into OpenAI Chat Completions chunks before
they reach the client, including function tool calls and terminal usage.

## Extending Nova Guard

Adding a new deterministic policy type is local:

1. Add a variant to `PolicyType` (`src/policy/policy_types.rs`) and a config
   struct.
2. Implement `PolicyRule` in `src/policy/rules/<your_rule>.rs`.
3. Add a `parse` arm in `compile_rule` (`src/policy/rules/mod.rs`).

The engine, middleware, telemetry, and synthetic-response layers pick it up with
no further changes.

## Cost / pricing

Per-request cost is computed from the versioned manifest in
`pricing/catalog.json`, mirrored into `src/policy/pricing_catalog.rs`, and used
by `src/policy/pricing.rs`. Catalog membership is a pricing capability, not a
claim that the caller's provider account can still invoke that model. Unknown
models use the compiled runtime's conservative maximum rather than $0, and
missing billable dimensions are bounded and marked incomplete. A zero-output
Anthropic pre-output refusal keeps token counts but settles at $0;
partial-output refusals are billed normally. Rates remain policy estimates, not
an invoice. The catalog version, current rows, update procedure, and primary
sources live in the single [pricing guide](PRICING.md).
