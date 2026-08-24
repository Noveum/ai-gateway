#!/usr/bin/env bash

set -euo pipefail

# CDPATH= is an intentional one-command prefix.
# shellcheck disable=SC1007
script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# CDPATH= is an intentional one-command prefix.
# shellcheck disable=SC1007
repository_root="$(CDPATH= cd -- "${script_dir}/.." && pwd)"
docker_context_validation_tmp=""
docker_runtime_validation_container=""
release_provenance_validation_tmp=""
release_image_validation_tmp=""
release_tag_preflight_tmp=""

package_version() {
  local version
  version="$({
    awk '
      /^\[package\][[:space:]]*$/ { in_package = 1; next }
      /^\[/ { if (in_package) exit; next }
      in_package && /^[[:space:]]*version[[:space:]]*=/ {
        line = $0
        sub(/^[^"]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
      }
    ' "${repository_root}/Cargo.toml"
  } || true)"

  if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
    echo "Could not read a valid package version from Cargo.toml" >&2
    return 1
  fi
  printf '%s\n' "${version}"
}

release_mode() {
  local event_name="$1"
  local git_ref="$2"

  if [[ "${event_name}" == "push" && "${git_ref}" == refs/tags/v* ]]; then
    local version expected_ref
    version="$(package_version)"
    expected_ref="refs/tags/v${version}"
    if [[ "${git_ref}" != "${expected_ref}" ]]; then
      echo "Release tag ${git_ref} does not match Cargo package version v${version}" >&2
      return 1
    fi
    printf 'release\n'
  else
    printf 'build\n'
  fi
}

self_test_release_mode() {
  local version
  version="$(package_version)"

  [[ "$(release_mode push "refs/tags/v${version}")" == "release" ]]
  [[ "$(release_mode push refs/heads/main)" == "build" ]]
  [[ "$(release_mode pull_request refs/pull/123/merge)" == "build" ]]
  [[ "$(release_mode workflow_dispatch refs/heads/main)" == "build" ]]

  if release_mode push "refs/tags/v${version}-mismatch" >/dev/null 2>&1; then
    echo "Mismatched release tag was accepted" >&2
    return 1
  fi
}

assert_release_provenance() {
  local candidate_ref="$1"
  local main_ref="$2"
  local git_repository="${3:-${repository_root}}"
  local candidate_sha main_sha

  candidate_sha="$(git -C "${git_repository}" rev-parse --verify "${candidate_ref}^{commit}")" || {
    echo "Release candidate ref does not resolve to a commit: ${candidate_ref}" >&2
    return 1
  }
  main_sha="$(git -C "${git_repository}" rev-parse --verify "${main_ref}^{commit}")" || {
    echo "Trusted main ref does not resolve to a commit: ${main_ref}" >&2
    return 1
  }
  if ! git -C "${git_repository}" merge-base --is-ancestor "${candidate_sha}" "${main_sha}"; then
    echo "Release commit ${candidate_sha} is not merged into trusted main ${main_sha}" >&2
    return 1
  fi
  echo "Release commit ${candidate_sha} is contained in trusted main ${main_sha}"
}

self_test_release_provenance() {
  local validation_tmp root_commit main_commit side_commit failure_output
  validation_tmp="$(mktemp -d "${TMPDIR:-/tmp}/noveum-release-provenance.XXXXXX")"
  release_provenance_validation_tmp="${validation_tmp}"
  cleanup_release_provenance_validation() {
    case "${release_provenance_validation_tmp}" in
      "${TMPDIR:-/tmp}"/noveum-release-provenance.*)
        rm -rf -- "${release_provenance_validation_tmp}"
        release_provenance_validation_tmp=""
        ;;
      "") ;;
      *)
        echo "Refusing to remove unexpected provenance path: ${release_provenance_validation_tmp}" >&2
        ;;
    esac
  }
  trap cleanup_release_provenance_validation EXIT HUP INT TERM

  git -C "${validation_tmp}" init --quiet
  git -C "${validation_tmp}" config user.name "Noveum release validator"
  git -C "${validation_tmp}" config user.email "release-validator@example.invalid"
  : >"${validation_tmp}/root"
  git -C "${validation_tmp}" add root
  git -C "${validation_tmp}" commit --quiet -m root
  git -C "${validation_tmp}" branch -M main
  root_commit="$(git -C "${validation_tmp}" rev-parse HEAD)"

  : >"${validation_tmp}/main"
  git -C "${validation_tmp}" add main
  git -C "${validation_tmp}" commit --quiet -m main
  main_commit="$(git -C "${validation_tmp}" rev-parse HEAD)"

  git -C "${validation_tmp}" switch --quiet --detach "${root_commit}"
  : >"${validation_tmp}/side"
  git -C "${validation_tmp}" add side
  git -C "${validation_tmp}" commit --quiet -m side
  side_commit="$(git -C "${validation_tmp}" rev-parse HEAD)"

  assert_release_provenance "${main_commit}" "${main_commit}" "${validation_tmp}"
  assert_release_provenance "${root_commit}" "${main_commit}" "${validation_tmp}"
  if failure_output="$(assert_release_provenance "${side_commit}" "${main_commit}" "${validation_tmp}" 2>&1)"; then
    echo "An unmerged release commit passed the provenance guard" >&2
    return 1
  fi
  if [[ "${failure_output}" != *"not merged into trusted main"* ]]; then
    echo "The unmerged release commit failed for an unexpected reason: ${failure_output}" >&2
    return 1
  fi

  cleanup_release_provenance_validation
  trap - EXIT HUP INT TERM
}

validate_workflow_wiring() {
  local build_job release_job preflight_line first_login_line first_push_line
  local release_mode_line release_provenance_line workflow_sensitive_count release_sensitive_count
  local docker_workflow provider_workflow classifier_line fetch_line provenance_line runtime_line first_publish_line
  local last_publish_line release_images_line metadata_version_priority metadata_latest_priority
  local build_action_count revision_arg_count smoke_if_line steps_line checkout_line first_secret_line
  docker_workflow="${repository_root}/.github/workflows/docker-build.yml"
  provider_workflow="${repository_root}/.github/workflows/provider-smoke.yml"

  build_job="$(awk '
    /^  docker:/ { in_job = 1 }
    in_job && /^  [A-Za-z0-9_-]+:/ && !/^  docker:/ { exit }
    in_job { print }
  ' "${docker_workflow}")"
  release_job="$(awk '
    /^  release:/ { in_job = 1 }
    in_job && /^  [A-Za-z0-9_-]+:/ && !/^  release:/ { exit }
    in_job { print }
  ' "${docker_workflow}")"
  if [[ "${build_job}" != *"contents: read"* || "${build_job}" == *"packages: write"* ||
    "${build_job}" == *'${{ secrets.'* || "${build_job}" == *"push: true"* ]]
  then
    echo "The build-validation job must have contents:read only and no publication credentials or pushes" >&2
    return 1
  fi
  if [[ -z "${release_job}" || "${release_job}" != *"needs: docker"* ||
    "${release_job}" != *"packages: write"* || "${release_job}" != *"contents: read"* ||
    "${release_job}" != *"refs/tags/v"* || "${release_job}" != *"concurrency:"* ||
    "${release_job}" != *"group:"* || "${release_job}" != *"cancel-in-progress: false"* ||
    "${release_job}" == *"environment:"* ]]
  then
    echo "A serialized tag-only release job must depend on the unprivileged Docker validation job" >&2
    return 1
  fi
  if printf '%s\n' "${build_job}" | grep -Eq '^    if:'; then
    echo "The unprivileged Docker validation job must run for every workflow event" >&2
    return 1
  fi

  workflow_sensitive_count="$(grep -Ec 'push:[[:space:]]*true|\$\{\{[[:space:]]*secrets\.' "${docker_workflow}" || true)"
  release_sensitive_count="$(printf '%s\n' "${release_job}" | grep -Ec 'push:[[:space:]]*true|\$\{\{[[:space:]]*secrets\.' || true)"
  if [[ "${workflow_sensitive_count}" -eq 0 || "${workflow_sensitive_count}" -ne "${release_sensitive_count}" ]]; then
    echo "All registry pushes and secrets must be isolated inside the tag-only release job" >&2
    return 1
  fi

  preflight_line="$(grep -n -m1 'validate_docker_release.sh release-tags-absent' "${docker_workflow}" | cut -d: -f1 || true)"
  first_login_line="$(grep -n -m1 'uses: docker/login-action@' "${docker_workflow}" | cut -d: -f1 || true)"
  first_push_line="$(grep -n -m1 'push:[[:space:]]*true' "${docker_workflow}" | cut -d: -f1 || true)"
  if [[ -z "${preflight_line}" || -z "${first_login_line}" || -z "${first_push_line}" ]] ||
    (( preflight_line >= first_login_line || preflight_line >= first_push_line ))
  then
    echo "Both immutable registry tags must be proven absent before any registry login or push" >&2
    return 1
  fi
  release_mode_line="$(grep -n 'validate_docker_release.sh mode' "${docker_workflow}" | tail -n 1 | cut -d: -f1 || true)"
  release_provenance_line="$(grep -n 'validate_docker_release.sh provenance HEAD refs/remotes/origin/main' "${docker_workflow}" | tail -n 1 | cut -d: -f1 || true)"
  if [[ -z "${release_mode_line}" || -z "${release_provenance_line}" ]] ||
    (( release_mode_line >= release_provenance_line || release_provenance_line >= preflight_line ))
  then
    echo "The release job must repeat exact-tag and trusted-main provenance checks before registry preflight" >&2
    return 1
  fi

  classifier_line="$(grep -n -m1 'validate_docker_release.sh mode' "${docker_workflow}" | cut -d: -f1 || true)"
  if [[ -z "${classifier_line}" ]]; then
    echo "Docker workflow does not invoke the release-mode validator" >&2
    return 1
  fi

  fetch_line="$(grep -n -m1 'git fetch --no-tags origin' "${docker_workflow}" | cut -d: -f1 || true)"
  provenance_line="$(grep -n -m1 'validate_docker_release.sh provenance HEAD refs/remotes/origin/main' "${docker_workflow}" | cut -d: -f1 || true)"
  runtime_line="$(grep -n -m1 'validate_docker_release.sh runtime-image' "${docker_workflow}" | cut -d: -f1 || true)"
  first_publish_line="$(grep -En -m1 'push:[[:space:]]*true|\$\{\{[[:space:]]*secrets\.' "${docker_workflow}" | cut -d: -f1 || true)"
  if [[ -z "${fetch_line}" || -z "${provenance_line}" || -z "${runtime_line}" || -z "${first_publish_line}" ]] ||
    (( fetch_line >= provenance_line || provenance_line >= first_publish_line || runtime_line >= first_publish_line ))
  then
    echo "Docker publication must follow runtime validation and an explicit trusted-main provenance check" >&2
    return 1
  fi

  metadata_version_priority="$(awk '
    /id: meta-ghcr/ { in_metadata = 1; next }
    in_metadata && /^      - name:/ { exit }
    in_metadata && /type=raw,value=\$\{\{ env\.CARGO_VERSION \}\},priority=[0-9]+/ {
      line = $0
      sub(/^.*priority=/, "", line)
      sub(/,.*/, "", line)
      print line
      exit
    }
  ' "${docker_workflow}")"
  metadata_latest_priority="$(awk '
    /id: meta-ghcr/ { in_metadata = 1; next }
    in_metadata && /^      - name:/ { exit }
    in_metadata && /type=raw,value=latest,priority=[0-9]+/ {
      line = $0
      sub(/^.*priority=/, "", line)
      sub(/,.*/, "", line)
      print line
      exit
    }
  ' "${docker_workflow}")"
  if [[ -z "${metadata_version_priority}" || -z "${metadata_latest_priority}" ]] ||
    (( metadata_version_priority <= metadata_latest_priority ))
  then
    echo "The immutable Cargo-version tag must outrank latest in GHCR metadata" >&2
    return 1
  fi

  last_publish_line="$(grep -En 'push:[[:space:]]*true' "${docker_workflow}" | tail -n 1 | cut -d: -f1 || true)"
  release_images_line="$(grep -n -m1 'validate_docker_release.sh release-images' "${docker_workflow}" | cut -d: -f1 || true)"
  if [[ -z "${last_publish_line}" || -z "${release_images_line}" ]] ||
    (( release_images_line <= last_publish_line ))
  then
    echo "Published release images must be anonymously pulled and validated after both registry pushes" >&2
    return 1
  fi

  build_action_count="$(grep -c 'uses: docker/build-push-action@' "${docker_workflow}" || true)"
  revision_arg_count="$(grep -c -F 'REVISION=${{ github.sha }}' "${docker_workflow}" || true)"
  if [[ "${build_action_count}" -eq 0 || "${revision_arg_count}" -ne "${build_action_count}" ]]; then
    echo "Every Docker build must receive github.sha as its OCI revision" >&2
    return 1
  fi

  smoke_if_line="$(grep -n -m1 "if: github.ref == 'refs/heads/main'" "${provider_workflow}" | cut -d: -f1 || true)"
  steps_line="$(grep -n -m1 '^[[:space:]]*steps:' "${provider_workflow}" | cut -d: -f1 || true)"
  checkout_line="$(grep -n -m1 'uses: actions/checkout@' "${provider_workflow}" | cut -d: -f1 || true)"
  first_secret_line="$(grep -n -m1 -F '${{ secrets.' "${provider_workflow}" | cut -d: -f1 || true)"
  if [[ -z "${smoke_if_line}" || -z "${steps_line}" || -z "${checkout_line}" || -z "${first_secret_line}" ]] ||
    (( smoke_if_line >= steps_line || steps_line >= checkout_line || checkout_line >= first_secret_line ))
  then
    echo "Provider smoke must reject non-main refs at the job boundary before checkout or secrets" >&2
    return 1
  fi

  if awk '
    /uses: actions\/checkout@/ { in_checkout = 1; next }
    in_checkout && /^      - / { exit(found_ref ? 0 : 1) }
    in_checkout && /^[[:space:]]+ref:/ { found_ref = 1 }
    END { if (in_checkout) exit(found_ref ? 0 : 1) }
  ' "${provider_workflow}"
  then
    echo "Provider smoke checkout overrides the trusted event SHA" >&2
    return 1
  fi

  self_test_release_mode
  self_test_release_provenance
  python3 "${repository_root}/scripts/test_registry_tag_preflight.py"
  echo "Workflow release and secret-ref guards are wired correctly"
}

validate_context() {
  if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required for build-context validation" >&2
    return 1
  fi
  if ! docker buildx version >/dev/null 2>&1; then
    echo "Docker Buildx is required for build-context validation" >&2
    return 1
  fi

  local validation_tmp context_dir positive_output positive_log
  validation_tmp="$(mktemp -d "${TMPDIR:-/tmp}/noveum-docker-context.XXXXXX")"
  docker_context_validation_tmp="${validation_tmp}"
  context_dir="${validation_tmp}/context"
  positive_output="${validation_tmp}/allowed-output"
  positive_log="${validation_tmp}/allowed-build.log"
  mkdir -p "${context_dir}"

  cleanup_context_validation() {
    case "${docker_context_validation_tmp}" in
      "${TMPDIR:-/tmp}"/noveum-docker-context.*)
        rm -rf -- "${docker_context_validation_tmp}"
        docker_context_validation_tmp=""
        ;;
      "") ;;
      *)
        echo "Refusing to remove unexpected validation path: ${docker_context_validation_tmp}" >&2
        ;;
    esac
  }
  trap cleanup_context_validation EXIT HUP INT TERM

  cp "${repository_root}/.dockerignore" "${context_dir}/.dockerignore"
  cp "${repository_root}/Cargo.toml" "${context_dir}/Cargo.toml"
  cp "${repository_root}/Cargo.lock" "${context_dir}/Cargo.lock"
  cp -R "${repository_root}/src" "${context_dir}/src"
  cp -R "${repository_root}/schema" "${context_dir}/schema"
  cp -R "${repository_root}/pricing" "${context_dir}/pricing"

  local allowed_dockerfile
  allowed_dockerfile=$'FROM scratch\nCOPY Cargo.toml /Cargo.toml\nCOPY Cargo.lock /Cargo.lock\nCOPY src /src\nCOPY schema /schema\nCOPY pricing /pricing'
  if ! printf '%s\n' "${allowed_dockerfile}" |
    docker buildx build --no-cache --progress=plain \
      --output "type=local,dest=${positive_output}" \
      --file - "${context_dir}" >"${positive_log}" 2>&1
  then
    echo "The Docker build context omitted a required release input" >&2
    sed 's/^/  /' "${positive_log}" >&2
    return 1
  fi

  local sentinel sentinel_path denied_log denied_output denied_dockerfile
  for sentinel in \
    ".env.test" \
    ".dev.vars" \
    "tests/.env.test" \
    ".wrangler/session.json" \
    "unlisted-release-sentinel.txt"
  do
    sentinel_path="${context_dir}/${sentinel}"
    mkdir -p "$(dirname -- "${sentinel_path}")"
    : >"${sentinel_path}"
    denied_log="${validation_tmp}/denied-$(printf '%s' "${sentinel}" | tr '/.' '__').log"
    denied_output="${validation_tmp}/denied-output-$(printf '%s' "${sentinel}" | tr '/.' '__')"
    denied_dockerfile="$(printf 'FROM scratch\nCOPY %s /forbidden-sentinel\n' "${sentinel}")"

    if printf '%s\n' "${denied_dockerfile}" |
      docker buildx build --no-cache --progress=plain \
        --output "type=local,dest=${denied_output}" \
        --file - "${context_dir}" >"${denied_log}" 2>&1
    then
      echo "Forbidden Docker context path was copyable: ${sentinel}" >&2
      return 1
    fi

    if ! grep -F "${sentinel}" "${denied_log}" >/dev/null 2>&1 ||
      ! grep -Eiq 'not found|excluded.*dockerignore|failed to calculate checksum' "${denied_log}"
    then
      echo "Docker rejected ${sentinel} for an unexpected reason" >&2
      sed 's/^/  /' "${denied_log}" >&2
      return 1
    fi
  done

  cleanup_context_validation
  trap - EXIT HUP INT TERM
  echo "Docker context contains only the required release inputs"
}

validate_runtime_image() {
  local image="$1"
  local expected_revision="${2:-}"
  local expected_version configured_user runtime_uid runtime_gid revision_label image_platform
  local container_name port_mapping host_port body

  if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required for runtime-image validation" >&2
    return 1
  fi
  expected_version="$(package_version)"
  image_platform="$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "${image}")"
  configured_user="$(docker image inspect --format '{{.Config.User}}' "${image}")"
  if [[ "${configured_user}" != "65532:65532" ]]; then
    echo "Runtime image must use fixed UID:GID 65532:65532, got: ${configured_user:-<empty>}" >&2
    return 1
  fi

  runtime_uid="$(docker run --rm --platform "${image_platform}" --entrypoint /usr/bin/id "${image}" -u)"
  runtime_gid="$(docker run --rm --platform "${image_platform}" --entrypoint /usr/bin/id "${image}" -g)"
  if [[ "${runtime_uid}" != "65532" || "${runtime_gid}" != "65532" ]]; then
    echo "Runtime image resolved to unexpected UID:GID ${runtime_uid}:${runtime_gid}" >&2
    return 1
  fi
  if [[ "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "${image}")" != "${expected_version}" ]]; then
    echo "Runtime image OCI version label does not match Cargo ${expected_version}" >&2
    return 1
  fi
  revision_label="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${image}")"
  if [[ -z "${revision_label}" || "${revision_label}" == "<no value>" ]]; then
    echo "Runtime image has no OCI source revision" >&2
    return 1
  fi
  if [[ -n "${expected_revision}" && "${revision_label}" != "${expected_revision}" ]]; then
    echo "Runtime image OCI revision ${revision_label} does not match ${expected_revision}" >&2
    return 1
  fi

  container_name="noveum-runtime-validation-$$-${RANDOM}"
  docker_runtime_validation_container="${container_name}"
  cleanup_runtime_validation() {
    if [[ -n "${docker_runtime_validation_container}" ]]; then
      docker rm --force "${docker_runtime_validation_container}" >/dev/null 2>&1 || true
      docker_runtime_validation_container=""
    fi
  }
  trap cleanup_runtime_validation EXIT HUP INT TERM

  docker run --detach \
    --name "${container_name}" \
    --platform "${image_platform}" \
    --publish 127.0.0.1::3000 \
    --read-only \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    "${image}" >/dev/null
  port_mapping="$(docker port "${container_name}" 3000/tcp | head -n 1)"
  host_port="${port_mapping##*:}"
  if [[ ! "${host_port}" =~ ^[0-9]+$ ]]; then
    echo "Could not resolve the validation container's published port" >&2
    docker logs "${container_name}" >&2 || true
    return 1
  fi

  body=""
  for _attempt in {1..60}; do
    if body="$(curl --fail --silent --show-error --connect-timeout 2 --max-time 5 \
      "http://127.0.0.1:${host_port}/health" 2>/dev/null)"; then
      break
    fi
    if [[ "$(docker inspect --format '{{.State.Running}}' "${container_name}" 2>/dev/null || true)" != "true" ]]; then
      echo "Validation container exited before becoming healthy" >&2
      docker logs "${container_name}" >&2 || true
      return 1
    fi
    sleep 0.25
  done
  if [[ "${body}" != *'"status":"healthy"'* || "${body}" != *"\"version\":\"${expected_version}\""* ]]; then
    echo "Validation container did not return the expected health/version body" >&2
    docker logs "${container_name}" >&2 || true
    return 1
  fi

  cleanup_runtime_validation
  trap - EXIT HUP INT TERM
  echo "Runtime image is healthy as UID:GID ${runtime_uid}:${runtime_gid} with revision ${revision_label} and a read-only root filesystem"
}

cleanup_release_tag_preflight() {
  case "${release_tag_preflight_tmp}" in
    "${TMPDIR:-/tmp}"/noveum-registry-preflight.*)
      rm -rf -- "${release_tag_preflight_tmp}"
      release_tag_preflight_tmp=""
      ;;
    "") ;;
    *)
      echo "Refusing to remove unexpected registry preflight path: ${release_tag_preflight_tmp}" >&2
      ;;
  esac
}

assert_manifest_absent() {
  local registry_base="$1"
  local token_url="$2"
  local service="$3"
  local repository="$4"
  local tag="$5"
  local display_image="$6"
  local validation_tmp token_body manifest_body token_status manifest_status token

  if ! command -v curl >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
    echo "curl and python3 are required for registry tag preflight" >&2
    return 1
  fi

  validation_tmp="$(mktemp -d "${TMPDIR:-/tmp}/noveum-registry-preflight.XXXXXX")"
  release_tag_preflight_tmp="${validation_tmp}"
  token_body="${validation_tmp}/token.json"
  manifest_body="${validation_tmp}/manifest.json"
  trap cleanup_release_tag_preflight EXIT HUP INT TERM

  if ! token_status="$(curl --silent --show-error --location \
    --connect-timeout 10 --max-time 30 --max-filesize 1048576 \
    --output "${token_body}" --write-out '%{http_code}' \
    --get \
    --data-urlencode "service=${service}" \
    --data-urlencode "scope=repository:${repository}:pull" \
    "${token_url}")"
  then
    echo "Anonymous registry token request failed for ${display_image}" >&2
    return 1
  fi
  if [[ "${token_status}" != "200" ]]; then
    echo "Anonymous registry token request failed for ${display_image} with HTTP ${token_status}" >&2
    return 1
  fi
  if ! token="$(python3 -c '
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
token = payload.get("token") or payload.get("access_token")
if not isinstance(token, str) or not token or any(ord(char) < 32 or ord(char) == 127 for char in token):
    raise SystemExit(1)
print(token, end="")
' "${token_body}" 2>/dev/null)"
  then
    echo "Anonymous registry token response was invalid for ${display_image}" >&2
    return 1
  fi

  if ! manifest_status="$(curl --silent --show-error --location \
    --connect-timeout 10 --max-time 30 --max-filesize 1048576 \
    --output "${manifest_body}" --write-out '%{http_code}' \
    --header "Authorization: Bearer ${token}" \
    --header 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
    "${registry_base%/}/v2/${repository}/manifests/${tag}")"
  then
    echo "Registry manifest request failed for ${display_image}" >&2
    return 1
  fi
  if [[ "${manifest_status}" == "200" ]]; then
    echo "Release tag already exists: ${display_image}" >&2
    return 1
  fi
  if [[ "${manifest_status}" != "404" ]]; then
    echo "Registry manifest status was ambiguous for ${display_image}: HTTP ${manifest_status}" >&2
    return 1
  fi
  if ! python3 -c '
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
errors = payload.get("errors")
if not isinstance(errors, list) or not any(
    isinstance(error, dict) and error.get("code") == "MANIFEST_UNKNOWN"
    for error in errors
):
    raise SystemExit(1)
' "${manifest_body}" 2>/dev/null
  then
    echo "Registry 404 did not return MANIFEST_UNKNOWN for ${display_image}" >&2
    return 1
  fi

  cleanup_release_tag_preflight
  trap - EXIT HUP INT TERM
  echo "Release tag is absent: ${display_image}"
}

validate_release_tags_absent() {
  local ghcr_image="$1"
  local dockerhub_image="$2"
  local expected_version ghcr_reference ghcr_repository ghcr_tag
  local dockerhub_repository dockerhub_tag

  expected_version="$(package_version)"
  if [[ "${ghcr_image}" != ghcr.io/*":${expected_version}" || "${ghcr_image}" == *@* ]]; then
    echo "GHCR preflight requires the exact immutable Cargo tag: ${ghcr_image}" >&2
    return 1
  fi
  if [[ "${dockerhub_image}" != *":${expected_version}" || "${dockerhub_image}" == *@* ||
    "${dockerhub_image}" == */*/* ]]
  then
    echo "Docker Hub preflight requires an owner/image Cargo tag: ${dockerhub_image}" >&2
    return 1
  fi

  ghcr_reference="${ghcr_image#ghcr.io/}"
  ghcr_tag="${ghcr_reference##*:}"
  ghcr_repository="${ghcr_reference%:*}"
  dockerhub_tag="${dockerhub_image##*:}"
  dockerhub_repository="${dockerhub_image%:*}"
  if [[ -z "${ghcr_repository}" || -z "${dockerhub_repository}" ||
    "${ghcr_tag}" != "${expected_version}" || "${dockerhub_tag}" != "${expected_version}" ]]
  then
    echo "Could not parse exact release image references" >&2
    return 1
  fi

  assert_manifest_absent \
    "${NOVEUM_RELEASE_GHCR_REGISTRY_BASE:-https://ghcr.io}" \
    "${NOVEUM_RELEASE_GHCR_TOKEN_URL:-https://ghcr.io/token}" \
    "ghcr.io" "${ghcr_repository}" "${ghcr_tag}" "${ghcr_image}" || return 1
  assert_manifest_absent \
    "${NOVEUM_RELEASE_DOCKERHUB_REGISTRY_BASE:-https://registry-1.docker.io}" \
    "${NOVEUM_RELEASE_DOCKERHUB_TOKEN_URL:-https://auth.docker.io/token}" \
    "registry.docker.io" "${dockerhub_repository}" "${dockerhub_tag}" "${dockerhub_image}" || return 1
  echo "Both immutable release tags are absent"
}

validate_release_images() {
  local ghcr_image="$1"
  local dockerhub_image="$2"
  local expected_revision="$3"
  local expected_version anonymous_config image

  if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required for release-image validation" >&2
    return 1
  fi
  expected_version="$(package_version)"
  for image in "${ghcr_image}" "${dockerhub_image}"; do
    if [[ "${image}" != *":${expected_version}" ]]; then
      echo "Release-image validation requires the immutable ${expected_version} tag, got: ${image}" >&2
      return 1
    fi
  done

  anonymous_config="$(mktemp -d "${TMPDIR:-/tmp}/noveum-anonymous-docker.XXXXXX")"
  release_image_validation_tmp="${anonymous_config}"
  cleanup_release_image_validation() {
    case "${release_image_validation_tmp}" in
      "${TMPDIR:-/tmp}"/noveum-anonymous-docker.*)
        rm -rf -- "${release_image_validation_tmp}"
        release_image_validation_tmp=""
        ;;
      "") ;;
      *)
        echo "Refusing to remove unexpected Docker config path: ${release_image_validation_tmp}" >&2
        ;;
    esac
  }
  trap cleanup_release_image_validation EXIT HUP INT TERM

  for image in "${ghcr_image}" "${dockerhub_image}"; do
    if ! docker --config "${anonymous_config}" pull --platform linux/amd64 "${image}"; then
      echo "Release image is not anonymously pullable: ${image}" >&2
      return 1
    fi
    if ! (validate_runtime_image "${image}" "${expected_revision}"); then
      echo "Published release image failed runtime or OCI metadata validation: ${image}" >&2
      return 1
    fi
  done

  cleanup_release_image_validation
  trap - EXIT HUP INT TERM
  echo "Both versioned release images are anonymously pullable and match Cargo ${expected_version} at ${expected_revision}"
}

usage() {
  cat >&2 <<'EOF'
Usage: scripts/validate_docker_release.sh package-version
       scripts/validate_docker_release.sh mode EVENT_NAME GIT_REF
       scripts/validate_docker_release.sh self-test-release-mode
       scripts/validate_docker_release.sh provenance [CANDIDATE_REF MAIN_REF]
       scripts/validate_docker_release.sh self-test-release-provenance
       scripts/validate_docker_release.sh workflows
       scripts/validate_docker_release.sh context
       scripts/validate_docker_release.sh runtime-image IMAGE [EXPECTED_REVISION]
       scripts/validate_docker_release.sh release-tags-absent GHCR_IMAGE DOCKERHUB_IMAGE
       scripts/validate_docker_release.sh release-images GHCR_IMAGE DOCKERHUB_IMAGE EXPECTED_REVISION
EOF
  return 2
}

command_name="${1:-}"
case "${command_name}" in
  package-version)
    [[ "$#" -eq 1 ]] || usage
    package_version
    ;;
  mode)
    [[ "$#" -eq 3 ]] || usage
    release_mode "$2" "$3"
    ;;
  self-test-release-mode)
    [[ "$#" -eq 1 ]] || usage
    self_test_release_mode
    ;;
  provenance)
    [[ "$#" -le 3 ]] || usage
    assert_release_provenance "${2:-HEAD}" "${3:-refs/remotes/origin/main}"
    ;;
  self-test-release-provenance)
    [[ "$#" -eq 1 ]] || usage
    self_test_release_provenance
    ;;
  workflows)
    [[ "$#" -eq 1 ]] || usage
    validate_workflow_wiring
    ;;
  context)
    [[ "$#" -eq 1 ]] || usage
    validate_context
    ;;
  runtime-image)
    [[ "$#" -ge 2 && "$#" -le 3 ]] || usage
    validate_runtime_image "$2" "${3:-}"
    ;;
  release-tags-absent)
    [[ "$#" -eq 3 ]] || usage
    validate_release_tags_absent "$2" "$3"
    ;;
  release-images)
    [[ "$#" -eq 4 ]] || usage
    validate_release_images "$2" "$3" "$4"
    ;;
  *)
    usage
    ;;
esac
