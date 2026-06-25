# Nova Guard — Policy Enforcement in the Gateway

Nova Guard is the policy-enforcement layer built into the Noveum AI Gateway. It
inspects every LLM request and response flowing through the gateway and can
**block**, **redact / mask**, or **flag** based on policies you define. It runs
the deterministic policy types entirely in-process (no network dependency), from
a **local policy bundle** (file or inline env) — the gateway enforces guardrails
standalone (BYOK mode) with no external service.

> Hosted policy distribution and budget reservation via the Noveum control plane
> are planned future work; today the gateway loads policies locally.

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

All of the types below are enforced in-process. The classifier types
(`scorer_gate`, `prompt_injection`, `topic_restriction`, `content_moderation`,
`grounding_check`) are reserved names that parse cleanly but are **not yet
enforced** (they would require an external scoring service); a bundle containing
them is accepted and those policies are skipped.

| Type | Phase(s) | What it does |
|---|---|---|
| `cost_cap` | input | Block/flag when project spend over a window crosses a cap (soft + hard). **Requires a cross-request state backend** (not bundled today); without one it fails open — see [Modes and fail-safety](#modes-and-fail-safety). |
| `rate_limit` | input | Block when requests/tokens over a window exceed a limit. **Requires a cross-request state backend** (not bundled today); without one it fails open. |
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
counters) to evaluate. No backend is bundled today, so they **always fail open**
(allow) — and a `failClosed: true` on those two types is neutralized at load time
with a warning, so a stale bundle can't block 100% of traffic. (The stateless
policy types — regex, PII, secrets, banned substrings, model allowlist, JSON
schema, token caps — enforce fully with no backend.)

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
