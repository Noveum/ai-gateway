# Nova Guard — Policy Enforcement in the Gateway

Nova Guard is the policy-enforcement layer built into the Noveum AI Gateway. It
inspects every LLM request and response flowing through the gateway and can
**block**, **redact / mask**, or **flag** based on policies you define. It runs
the deterministic policy types entirely in-process (no network dependency), so
the gateway enforces guardrails standalone (BYOK mode) as well as when wired to
the Noveum control plane for hosted policies and atomic budget reservation.

> Nova Guard in the gateway is one of three enforcement surfaces (the others are
> the `noveum-trace` Python SDK and the litellm patch) that share one control
> plane. A policy authored once enforces identically on whichever surface your
> traffic uses.

## Quick start

1. Write a policy bundle (`nova-guard.json`) — see [Policy bundle](#policy-bundle).
2. Point the gateway at it:

   ```bash
   export NOVEUM_GUARD_ENABLED=true
   export NOVEUM_GUARD_POLICIES_FILE=./nova-guard.json
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
| `NOVEUM_ENDPOINT` / `NOVEUM_API_KEY` | _(unset)_ | Control-plane endpoint + key (enables hosted policy fetch + budget reservation). |
| `ENABLE_NOVEUM_TRACES` | `false` | Ship per-request traces to the Noveum platform's `/v1/traces`. |

When the engine is disabled or has zero active policies, the middleware
short-circuits without buffering the body, so guardrails add **no overhead** when
unused.

## Policy bundle

The bundle format (`nova-guard.json`) is shared with the Nova Guard SDK and
control plane:

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

Deterministic types are enforced in-process; the v1.5 classifier types
(`scorer_gate`, `prompt_injection`, `topic_restriction`, `content_moderation`,
`grounding_check`) are recognized and routed to the NovaEval scoring service
(shared with the SDK).

| Type | Phase(s) | What it does |
|---|---|---|
| `cost_cap` | input | Block/flag when project spend over a window crosses a cap (soft + hard). Strict mode reserves atomically via the control plane. |
| `rate_limit` | input | Block when requests/tokens over a window exceed a limit. |
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

For `cost_cap` / `rate_limit`, when live state is unavailable the policy fails
**open** (allows) by default; set `failClosed: true` (or `enforcementMode:
strict` with the control plane) to fail **closed** (block).

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
