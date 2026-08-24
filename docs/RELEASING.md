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
- The GHCR package is explicitly **Public** in package settings. Repository
  visibility does not make a container package public; GitHub documents the
  separate setting in
  [Configuring a package's access control and visibility](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility).
  GitHub also documents that changing a package to Public cannot be undone, so
  obtain the package owner's explicit approval and record the change. This
  workflow validates visibility but never changes it.
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

The immutable Cargo-version tag has higher metadata priority than `latest`, so
the generated OCI version label must equal the Cargo version. After both
pushes, the workflow uses an empty temporary Docker configuration to
anonymously pull each exact version tag and reruns the hardened runtime/OCI
validator. This is the release gate for public visibility and registry
contents; an authenticated push alone is not sufficient.

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

### v2.0.1 container exception

The 2026-08-24 GHCR v2.0.1 publication used equal-priority raw tags with
`latest` first. Its immutable image therefore reports
`org.opencontainers.image.version=latest`, and anonymous access failed during
the release audit. Docker Hub v2.0.1 is public and correctly labeled. Do not
rerun the tag by moving/recreating it, and do not overwrite either versioned
image. Changing the existing GHCR package visibility to Public is a separate
administrative action that preserves its digest, but it does not repair the
label; corrected GHCR metadata belongs in a later patch release.

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
