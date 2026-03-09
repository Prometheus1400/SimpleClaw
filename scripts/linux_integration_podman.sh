#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
IMAGE_TAG="${SIMPLECLAW_LINUX_TEST_IMAGE:-simpleclaw-linux-build}"
BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
RUSTFLAGS_VALUE="${RUSTFLAGS:--C debuginfo=0 -C codegen-units=1 -C link-arg=-Wl,--no-keep-memory -C link-arg=-Wl,--reduce-memory-overheads}"
PODMAN_MEMORY="${PODMAN_MEMORY:-}"
PODMAN_CPUS="${PODMAN_CPUS:-}"
SKIP_IMAGE_BUILD="${SKIP_IMAGE_BUILD:-0}"
REUSE_CONTAINER="${REUSE_CONTAINER:-1}"
CONTAINER_NAME="${CONTAINER_NAME:-simpleclaw-linux-test}"

CARGO_REGISTRY_VOL="${CARGO_REGISTRY_VOL:-simpleclaw-cargo-registry}"
CARGO_GIT_VOL="${CARGO_GIT_VOL:-simpleclaw-cargo-git}"
TARGET_VOL="${TARGET_VOL:-simpleclaw-target-linux}"

if ! command -v podman >/dev/null 2>&1; then
  echo "podman is required but was not found in PATH" >&2
  exit 1
fi

if [[ "${SKIP_IMAGE_BUILD}" != "1" ]]; then
  echo "[linux-test] building image ${IMAGE_TAG}"
  podman build -t "${IMAGE_TAG}" -f "${REPO_ROOT}/Containerfile.linux-build" "${REPO_ROOT}"
else
  echo "[linux-test] skipping image build (SKIP_IMAGE_BUILD=1)"
fi

RUN_ARGS=(
  -i
  -e "CARGO_BUILD_JOBS=${BUILD_JOBS}"
  -e "CARGO_INCREMENTAL=0"
  -e "RUSTFLAGS=${RUSTFLAGS_VALUE}"
  -e "CARGO_TARGET_DIR=/workspace/target-linux"
  -v "${CARGO_REGISTRY_VOL}:/usr/local/cargo/registry"
  -v "${CARGO_GIT_VOL}:/usr/local/cargo/git"
  -v "${TARGET_VOL}:/workspace/target-linux"
  -v "${REPO_ROOT}:/workspace"
  -w /workspace
)

if [[ -n "${PODMAN_MEMORY}" ]]; then
  RUN_ARGS+=(--memory "${PODMAN_MEMORY}")
fi

if [[ -n "${PODMAN_CPUS}" ]]; then
  RUN_ARGS+=(--cpus "${PODMAN_CPUS}")
fi

TEST_CMD=(
  bash -lc
  "set -euo pipefail
   export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:\${PATH}
   echo \"[linux-test] CARGO_BUILD_JOBS=\${CARGO_BUILD_JOBS} RUSTFLAGS=\${RUSTFLAGS}\"
   which cargo
   cargo --version
   which bwrap
   which socat
   bwrap --version
   socat -V >/dev/null
   cargo test -j \"\${CARGO_BUILD_JOBS}\" --test gateway_roundtrip_e2e -- --nocapture"
)

if [[ -n "${SANDBOX_RUNTIME_PATH:-}" ]]; then
  echo "[linux-test] using sandbox-runtime path override: ${SANDBOX_RUNTIME_PATH}"
  RUN_ARGS+=(-v "${SANDBOX_RUNTIME_PATH}:/sandbox-runtime-rs")
  TEST_CMD=(
    bash -lc
    "set -euo pipefail
     export PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:\${PATH}
     echo \"[linux-test] CARGO_BUILD_JOBS=\${CARGO_BUILD_JOBS} RUSTFLAGS=\${RUSTFLAGS}\"
     which cargo
     cargo --version
     which bwrap
     which socat
     bwrap --version
     socat -V >/dev/null
     cargo test -j \"\${CARGO_BUILD_JOBS}\" --config 'patch.crates-io.sandbox-runtime.path=\"/sandbox-runtime-rs\"' --test gateway_roundtrip_e2e -- --nocapture"
  )
fi

echo "[linux-test] running integration tests in container"
if [[ "${REUSE_CONTAINER}" == "1" ]]; then
  if ! podman container exists "${CONTAINER_NAME}"; then
    echo "[linux-test] creating reusable container: ${CONTAINER_NAME}"
    podman run -d --name "${CONTAINER_NAME}" --privileged \
      "${RUN_ARGS[@]}" "${IMAGE_TAG}" bash -lc "sleep infinity" >/dev/null
  else
    if [[ "$(podman inspect -f '{{.State.Running}}' "${CONTAINER_NAME}")" != "true" ]]; then
      echo "[linux-test] starting existing container: ${CONTAINER_NAME}"
      podman start "${CONTAINER_NAME}" >/dev/null
    fi
  fi
  podman exec -t "${CONTAINER_NAME}" "${TEST_CMD[@]}"
else
  podman run --rm -t --privileged "${RUN_ARGS[@]}" "${IMAGE_TAG}" "${TEST_CMD[@]}"
fi
