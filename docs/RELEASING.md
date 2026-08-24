# Release procedure

This runbook is for maintainers publishing the Rust crate and matching GitHub
release. A crates.io version is immutable: it cannot be overwritten or deleted.
A bad version can be yanked from new dependency resolution, but existing
lockfiles and downloads remain valid.

## Prerequisites

- The implementation is reviewed and merged to `main`.
- Required control-plane and service deployments are complete.
- The version follows SemVer; a public Rust API break requires a major release.
- `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and comparison links agree.
- The packaged README links repository documentation to the exact release tag,
  not `HEAD` or a moving branch.
- crates.io credentials are stored in Cargo's credential provider or a scoped
  `CARGO_REGISTRY_TOKEN`, never in the repository or command history.

## 1. Validate the exact main commit

```bash
git switch main
git pull --ff-only origin main
git status --short
git rev-parse HEAD
```

Run the complete [validation checklist](VALIDATION.md), including strict
rustdoc, Worker/workerd, package, container, and authorized live-provider gates.
Then inspect the package itself:

```bash
cargo package --locked --list
cargo publish --locked --dry-run
bash scripts/validate_docker_release.sh workflows
bash scripts/validate_docker_release.sh context
```

The dry-run verifies the crate archive and crates.io metadata. It does not prove
that a Cloudflare, Docker, Kubernetes, or control-plane deployment works. The
Docker checks prove that PR, `main`, and manual events are build-only, that a
mismatched release tag or a tag outside trusted `main` is rejected, and that the
deny-by-default context admits only the release inputs copied by the Dockerfile.
The workflow-wiring check also proves that credential-bearing provider smoke
is job-gated to `refs/heads/main` before checkout or secret exposure, then
checks out the exact scheduled/manual event SHA.

## 2. Publish once

Reconfirm that the tree is clean and still equals `origin/main` immediately
before publication:

```bash
git fetch origin main
test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"
test -z "$(git status --porcelain)"
cargo publish --locked
```

Do not retry blindly after a timeout. Check crates.io first: the upload may have
succeeded even if the local client lost the response.

## 3. Verify, tag, and create the GitHub release

Only after crates.io shows the version. The commands below use 2.0.1 as the
release example; preparing this runbook does not mean the version has already
been published:

```bash
NOVEUM_RELEASE_VERSION=2.0.1
git tag -a "v${NOVEUM_RELEASE_VERSION}" -m "v${NOVEUM_RELEASE_VERSION}"
git push origin "v${NOVEUM_RELEASE_VERSION}"
gh release create "v${NOVEUM_RELEASE_VERSION}" --verify-tag \
  --title "v${NOVEUM_RELEASE_VERSION}" --generate-notes
```

The tag push is the **only** publishing trigger for release containers. The
Docker workflow verifies that the ref is exactly
`v${Cargo package version}` and that its commit is contained in trusted `main`
before logging into either registry, then publishes `${NOVEUM_RELEASE_VERSION}`
and `latest` to both
`ghcr.io/noveum/ai-gateway` and `noveum/noveum-ai-gateway`. Pull requests,
`main` pushes, and manual workflow runs never publish. Wait for that exact tag's
workflow to finish, then verify both versioned images and record their digests;
do not infer success from the moving `latest` tag.

Verify all of the following resolve to the same version and source commit:

- crates.io version and checksum;
- docs.rs build;
- Git tag;
- GitHub release; and
- GHCR and Docker Hub version tags, OCI version/source-revision labels, fixed
  `65532:65532` runtime identity, and recorded digests;
- production runtime `/health` response after service deployment.

For v2.0.1, do not declare completion until the exact version is visible from
the generic
[crates.io package](https://crates.io/crates/noveum-ai-gateway),
[docs.rs package](https://docs.rs/noveum-ai-gateway), and
[GitHub releases page](https://github.com/Noveum/ai-gateway/releases).

## Emergency response

Roll back service deployments using their recorded immutable artifact. If the
crate itself must not be selected by new builds:

```bash
NOVEUM_BAD_VERSION=2.0.1
cargo yank --vers "$NOVEUM_BAD_VERSION" noveum-ai-gateway
```

Document why the version was yanked and publish a corrected new version. Never
move or recreate an existing release tag, overwrite a versioned container tag,
or attempt to replace an existing crate archive.
