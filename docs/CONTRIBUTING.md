# Contributing to Noveum AI Gateway

Contributions are welcome. Start with a GitHub issue or discussion when a
change affects the public API, provider compatibility, policy behavior,
pricing, tenancy, or deployment security; those contracts need agreement before
implementation.

## Set up

The minimum supported Rust version is 1.94.1. Worker changes also require Node
22+, the `wasm32-unknown-unknown` target, pinned `worker-build 0.8.5`, and
Wrangler 4.120.0.

```bash
git clone https://github.com/Noveum/ai-gateway.git
cd ai-gateway
git switch -c feature/short-description
cargo metadata --locked --format-version 1 >/dev/null
cargo test --locked --lib
```

Read the [documentation index](README.md), the relevant provider/architecture
guide, and [the v2 migration notes](MIGRATING_TO_V2.md) before changing a public
type or behavior.

## Make the change

- Keep provider behavior explicit. Do not claim upstream capabilities the
  adapter does not translate or a live account has not verified.
- Preserve transparent proxy compatibility unless a documented strict policy
  contract requires a narrower request shape.
- Fail visibly on guard configuration that would otherwise look enabled while
  enforcing nothing.
- Never add real credentials, `.env` files, production payloads, or secret
  values to fixtures/logs.
- Update `CHANGELOG.md` for user-visible behavior and link rather than
  duplicating an existing authoritative guide.
- Pricing changes start in `pricing/catalog.json`, must cite a primary provider
  source, bump the catalog version, update every runtime mirror, and add exact
  parity/boundary tests. See [Pricing](PRICING.md#updating-prices).

## Validate

Run the checks relevant to the change. The complete commands and live-test
rules are in [Validation](VALIDATION.md). The minimum Rust gate is:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --lib
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
git diff --check
```

Worker changes also need the wasm check, pinned release bundle, Wrangler
dry-run, and 13-phase workerd suite. A mocked test is not a claim that a paid
provider or production deployment passed; record live coverage and omissions
separately.

## Open the pull request

Include:

- the problem and intended user-visible outcome;
- security, tenancy, pricing, and compatibility implications;
- exact commands/results and the tested commit SHA;
- live providers/runtimes tested, with explicit “not tested” entries;
- rollout and rollback plan for deployment-affecting changes; and
- documentation/changelog updates.

Keep the branch focused, address review feedback with tests where practical,
and rerun exact-head checks after the final change.

## Releases and roadmap

Maintainers publish only reviewed, clean `main` commits using the immutable
[release procedure](RELEASING.md). Candidate ideas belong in an issue; the
[roadmap](TODO.md) lists known gaps but is not a release promise.
