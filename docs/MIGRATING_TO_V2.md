# Migrating from v1.2 to v2.0

Noveum AI Gateway 2.0.0 is a SemVer-major release. Binary users can usually
upgrade by replacing the executable and validating configuration. Rust library
users must update three public APIs.

Upgrade to the latest 2.0 patch available in your artifact channel. The public
API changes were introduced in 2.0.0; version 2.0.1 adds post-release hardening
without another API break.

- [crates.io package](https://crates.io/crates/noveum-ai-gateway)
- [docs.rs package](https://docs.rs/noveum-ai-gateway)
- [GitHub releases](https://github.com/Noveum/ai-gateway/releases)
- [v2.0.1 hardening notes](../CHANGELOG.md#201---2026-08-24)
- [v2.0.0 breaking release notes](../CHANGELOG.md#200---2026-08-24)

## Before upgrading

1. Upgrade the toolchain to Rust **1.94.1** or newer.
2. Record the current binary/image/Worker version so rollback has an exact
   target.
3. Inventory all `NOVEUM_GUARD_*` and `NOVEUM_*` settings. Empty or half-applied
   bridge settings are errors in v2; do not rely on them being ignored.
4. Decide whether the deployment is transparent, local-policy, dedicated, or
   native shared tenancy. See [Configuration](configuration.md#choose-one-mode).
5. If organization-scoped policies are enabled, deploy the Noveum control-plane
   composite organization state support before the v2 gateway.

## Dependency and MSRV

Pin the major or exact version according to your update policy:

```toml
[dependencies]
noveum-ai-gateway = "2"
```

Then update and verify the locked dependency graph:

```bash
cargo update -p noveum-ai-gateway --precise 2.0.1
cargo check --locked --all-targets
cargo test --locked
```

## Public API changes

### `AppState::new` has four additional components

The v1.2 constructor accepted configuration, metrics, and the policy engine.
The v2 constructor also accepts the optional live-state provider, usage
reporter, atomic-admission client, and shared-tenancy resolver:

```rust
let state = AppState::new(
    config,
    metrics,
    policy,
    None, // RemoteLiveState
    None, // UsageReporter
    None, // AdmissionClient
    None, // SharedTenancy
);
```

Passing four `None` values preserves a local/transparent embedding. Applications
that enable the platform bridge should construct these components using the
same validated configuration path as the binary; do not create only part of a
bridge and assume the missing component will fail open.

### `Policy::policy_type` preserves the wire value

The field changed from `PolicyType` to `PolicyTypeTag`. Existing pattern matches
must call `kind()`:

```rust
match policy.kind() {
    PolicyType::RegexMatch => { /* ... */ }
    other => { /* ... */ }
}
```

Use `policy.type_name()` when reporting the exact value received in JSON. This
is important for unknown or mistyped policy types: v2 can now report the
operator's original string instead of collapsing it to `unknown`.

Code that constructs a `Policy` directly can convert a known enum:

```rust
policy.policy_type = PolicyType::RegexMatch.into();
```

The serialized policy contract is unchanged: JSON still carries a string in
the `type` field.

### `PolicyEngine::from_env` now returns `Result`

The function remains **asynchronous**. In v1.2 it returned `Self`; in v2 it
returns `Result<Self, String>` so a configured but unreadable, malformed, or
unenforceable bundle cannot silently become a pass-through engine:

```rust
let engine = PolicyEngine::from_env().await?;
```

If the surrounding error type is not compatible with `String`, map the error
and abort startup. Do not replace the error with `PolicyEngine::disabled()` in
production unless intentionally entering an emergency unguarded state.

## Behavioral changes operators must validate

- Platform-managed Nova Guard supports project and organization live state,
  atomic strict admission, and reservation completion/cancel/abandon.
- Native shared tenancy authenticates each request with `x-noveum-api-key` and
  derives its project/organization server-side. The Cloudflare Worker refuses
  shared tenancy.
- An applicable enforcing/blocking strict cost cap requires a positive explicit
  output limit. An unbounded request returns HTTP 400 `missing_output_limit`
  before provider dispatch.
- Unknown models are conservatively priced rather than treated as free. A
  fail-closed cost cap can reject an unknown model.
- Streaming responses are metered for settlement, but output-phase content
  enforcement remains skipped for SSE in v2.0.x.
- Local/inline stateful `cost_cap` and `rate_limit` policies still need the
  platform bridge. The Worker refuses such an inline bundle because it has no
  cross-request state backend.

Review [Nova Guard](NOVA_GUARD.md) and [Pricing](PRICING.md) before enabling a
hard budget.

## Rollout and rollback

1. Run the [validation checklist](VALIDATION.md) on the exact candidate.
2. Deploy to a non-production endpoint or zero-traffic Worker version.
3. Confirm `/health` reports the exact candidate version.
4. Test one bounded buffered call and one bounded SSE call with low token
   ceilings.
5. For a guarded deployment, verify policy fetch, state scope, admission,
   settlement, and no active reservation remains.
6. Promote gradually where the runtime supports it and watch gateway, provider,
   and control-plane errors.

Rollback the deployment artifact, not just its configuration. Cloudflare
operators should use the recorded previous immutable Worker version; native
operators should restore the previous pinned image or binary. Re-run the same
health and provider probes after rollback. A rollback does not erase usage or
reservation records already written to the control plane.
