# Native, Docker, and Kubernetes deployment

This guide covers the native Axum server. For the edge runtime, use the
[Cloudflare Worker runbook](CLOUDFLARE_WORKER.md).

## Production contract

- The native process serves `GET /`, `HEAD /`, `GET /health`, and `/v1/*`.
- Beginning with v2.0.1, `GET /` is a static, no-JavaScript information page
  with a restrictive CSP and five-minute cache; it is not an administrative
  console. `HEAD /` returns the same status and headers with an empty body.
- `GET /health` is the only health route in v2.0.1. It reports process health
  and version, not provider credentials or control-plane reachability. There is
  no `/ready` endpoint.
- Provider credentials arrive per request. A dedicated Noveum service key is a
  deployment secret; a native shared caller key arrives per request.
- Transparent and dedicated modes do not authenticate general callers. Put the
  service behind TLS and an access-control layer when it must be private.

Read [Configuration](configuration.md) and choose the tenancy mode before
creating a manifest.

## Run the binary

```bash
cargo install noveum-ai-gateway --version 2.0.1 --locked
HOST=0.0.0.0 PORT=3000 RUST_LOG=info noveum-ai-gateway
```

Version 2.0.1 honors `HOST` and defaults to loopback. Container and pod
deployments must set `HOST=0.0.0.0` so their port or probe can reach the
process. Version 2.0.0 had a binding bug and always listened on the wildcard
address, so retain an external network boundary during an upgrade.

For a system service, run as an unprivileged user, inject environment variables
from the host secret manager, set restart limits, and place a TLS/authenticated
reverse proxy in front. Do not place provider keys in the service environment;
callers supply them per request.

## Docker

Release automation builds linux/amd64 on pull requests, `main`, and manual
runs, but those events publish nothing. It publishes the Cargo version and
`latest` to both `ghcr.io/noveum/ai-gateway` and
`noveum/noveum-ai-gateway` only when a pushed Git tag exactly matches
`v${Cargo package version}` **and** that tag's commit is already contained in
trusted `main`. A mismatched or unreviewed `v*` tag fails before registry login.
For this release, only `v2.0.1` on the reviewed mainline can publish the `2.0.1`
images. A release-preparation commit or crate publication therefore does not
prove that the container exists: confirm the versioned tag and digest in the
intended registry before rollout, then pin that digest for production. `latest`
moves only on a matching release tag, but remains mutable and is not a release
identifier.

GHCR package visibility is separate from repository visibility. A supported
public image must be anonymously pullable, not merely pullable by the release
workflow's `GITHUB_TOKEN`. The release workflow therefore uses an empty
temporary Docker configuration after publication to pull each exact GHCR and
Docker Hub version tag, then repeats the OCI-label, runtime-identity,
read-only-root, and health checks on those registry artifacts.

### v2.0.1 registry caveat

The immutable GHCR v2.0.1 image published on 2026-08-24 has
`org.opencontainers.image.version=latest`; anonymous access also failed during
the release verification. Docker Hub's v2.0.1 image is publicly pullable and
has the correct `2.0.1` version label and source revision. Do not repush,
retag, or recreate v2.0.1. A package administrator may separately make the
existing GHCR package public without changing its digest, but only a later
patch release can provide corrected GHCR OCI metadata. Until then, use the
verified Docker Hub digest when exact container metadata is required.

The repository's `.dockerignore` denies the entire working tree, then admits
only `Cargo.toml`, `Cargo.lock`, `src/`, `schema/`, and `pricing/`. Docker still
receives the selected Dockerfile separately. This keeps `.env` files, Wrangler
state, generated bundles, logs, tests, documentation, and unrelated local files
out of local and remote BuildKit contexts. The release validation script proves
both the required allowlist and representative forbidden paths before CI builds
an image.

```bash
docker pull --platform linux/amd64 noveum/noveum-ai-gateway:2.0.1
docker run --detach \
  --name noveum-ai-gateway \
  --publish 127.0.0.1:3000:3000 \
  --env HOST=0.0.0.0 \
  --env PORT=3000 \
  --env RUST_LOG=info \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  noveum/noveum-ai-gateway:2.0.1
```

Bind the published port to a private interface or localhost and proxy it through
TLS. Verify the exact running version:

```bash
curl --fail --silent http://127.0.0.1:3000/health
docker logs --since 5m noveum-ai-gateway
```

The release image declares the fixed unprivileged identity `65532:65532`. CI
starts that exact identity with a read-only root filesystem, all capabilities
dropped, and `no-new-privileges`, then requires `/health` to report the Cargo
version. Read-only policy or configuration mounts must be readable by UID/GID
65532, and every parent directory must be traversable by that identity. Set
ownership and permissions deliberately before rollout; do not solve a mount
error by adding `--user 0`. Any `--user` override departs from the tested
default and needs an explicit security review.

### Local policy bundle

Mount a read-only file instead of baking it into the image:

```bash
docker run --detach \
  --name noveum-ai-gateway \
  --publish 127.0.0.1:3000:3000 \
  --env HOST=0.0.0.0 \
  --env NOVEUM_GUARD_POLICIES_FILE=/etc/noveum/nova-guard.json \
  --mount type=bind,src="$(pwd)/nova-guard.json",dst=/etc/noveum/nova-guard.json,readonly \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  noveum/noveum-ai-gateway:2.0.1
```

For a dedicated platform bridge, inject `NOVEUM_API_KEY` from a secret manager
and set `NOVEUM_GUARD_PROJECT_ID`; do not use a command-line `--env
NOVEUM_API_KEY=value`, which can expose the value in shell history and process
metadata.

## Kubernetes

The example below is transparent. Add an authenticated Ingress/Gateway and
NetworkPolicy for production exposure. Replace the image tag with the verified
digest used by your release system.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: noveum-ai-gateway
spec:
  replicas: 3
  revisionHistoryLimit: 5
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 0
      maxSurge: 1
  selector:
    matchLabels:
      app: noveum-ai-gateway
  template:
    metadata:
      labels:
        app: noveum-ai-gateway
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        fsGroup: 65532
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: gateway
          image: noveum/noveum-ai-gateway:2.0.1
          imagePullPolicy: IfNotPresent
          ports:
            - name: http
              containerPort: 3000
          env:
            - name: HOST
              value: "0.0.0.0"
            - name: PORT
              value: "3000"
            - name: RUST_LOG
              value: "info"
            - name: DEPLOYMENT_ENVIRONMENT
              value: "production"
          startupProbe:
            httpGet:
              path: /health
              port: http
            failureThreshold: 30
            periodSeconds: 2
          readinessProbe:
            httpGet:
              path: /health
              port: http
            periodSeconds: 5
            timeoutSeconds: 2
          livenessProbe:
            httpGet:
              path: /health
              port: http
            periodSeconds: 10
            timeoutSeconds: 2
          resources:
            requests:
              cpu: 250m
              memory: 256Mi
            limits:
              cpu: "1"
              memory: 1Gi
          securityContext:
            allowPrivilegeEscalation: false
            capabilities:
              drop: ["ALL"]
            readOnlyRootFilesystem: true
---
apiVersion: v1
kind: Service
metadata:
  name: noveum-ai-gateway
spec:
  selector:
    app: noveum-ai-gateway
  ports:
    - name: http
      port: 80
      targetPort: http
  type: ClusterIP
```

Because `/health` is process-only, a pod can be ready while a particular
provider key is invalid or the upstream is degraded. Monitor real, low-cost
synthetic calls separately. A guarded deployment should also monitor policy
fetch, admission, and settlement outcomes.

### Dedicated bridge secret example

Create the Secret through the cluster's approved secret provider. Reference it
without putting the value in this manifest:

```yaml
env:
  - name: NOVEUM_API_KEY
    valueFrom:
      secretKeyRef:
        name: noveum-gateway
        key: api-key
  - name: NOVEUM_GUARD_PROJECT_ID
    value: "project-id"
  - name: NOVEUM_GUARD_TENANCY
    value: "dedicated"
```

The service key needs `guardrails:read` and `guardrails:ingest`. Shared mode has
no process-wide key; callers need those scopes plus `projects:read`.

## Rollout

1. Run the [validation checklist](VALIDATION.md) on the exact image source.
2. Record the current image and ReplicaSet revision.
3. Apply the candidate and wait for rollout completion.
4. Confirm the in-cluster and external `/health` versions.
5. Run low-cost buffered and streaming provider probes through the production
   route.
6. Observe HTTP errors, provider errors, latency, restarts, and Nova Guard
   settlement before increasing traffic.

```bash
kubectl get deployment noveum-ai-gateway \
  -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
kubectl rollout history deployment/noveum-ai-gateway
kubectl rollout status deployment/noveum-ai-gateway --timeout=5m
kubectl get pods -l app=noveum-ai-gateway
kubectl logs deployment/noveum-ai-gateway --since=10m
```

## Rollback

If validation fails, stop the rollout and restore the recorded artifact:

```bash
kubectl rollout undo deployment/noveum-ai-gateway
kubectl rollout status deployment/noveum-ai-gateway --timeout=5m
```

Then repeat the external health and provider probes. Verify no temporary policy,
active reservation, or half-applied secret remains. For Docker, start the
previous version under a new container name, verify it on a private port, switch
the reverse proxy, and only then remove the failed container.

## Observability and alerts

- Set `RUST_LOG=info` for normal operations; use targeted debug filters briefly.
- Keep `DEBUG_METRICS=false` unless prompt/response content is safe to print.
- Collect process exits/restarts, HTTP status, provider status, latency, TTFB,
  token/cost completeness, Nova Guard blocks, admission failures, abandoned
  reservations, and usage-queue warnings.
- Alert on sustained 5xx, control-plane fail-closed blocks, settlement retry
  exhaustion, and any mismatch between intended and reported `/health` version.

See [Telemetry and log handling](logs.md) for the exported record and its data
handling implications.

## Production security checklist

- [ ] TLS and caller authentication terminate before the gateway.
- [ ] Sensitive headers are redacted from load-balancer, ingress, and APM logs.
- [ ] CORS exposure is appropriate for the authenticated frontend.
- [ ] Runtime runs unprivileged with a read-only filesystem and least privilege.
- [ ] Secrets come from a secret manager and are absent from images/manifests.
- [ ] Provider quotas, AWS IAM, and billing alerts are enabled independently of
  Nova Guard.
- [ ] Network egress is limited to intended provider and control-plane origins
  where the platform permits it.
- [ ] Rollback artifact and command have been tested.
