#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${ROOT_DIR}/target/wasm-guests"
ASSET_DIR="${ROOT_DIR}/assets/wasm"

mkdir -p "${TARGET_DIR}" "${ASSET_DIR}"

cargo build \
  --manifest-path "${ROOT_DIR}/guests/read_guest/Cargo.toml" \
  --target wasm32-wasip1 \
  --release \
  --target-dir "${TARGET_DIR}"

cargo build \
  --manifest-path "${ROOT_DIR}/guests/edit_guest/Cargo.toml" \
  --target wasm32-wasip1 \
  --release \
  --target-dir "${TARGET_DIR}"

cp "${TARGET_DIR}/wasm32-wasip1/release/read_guest.wasm" "${ASSET_DIR}/read_guest.wasm"
cp "${TARGET_DIR}/wasm32-wasip1/release/edit_guest.wasm" "${ASSET_DIR}/edit_guest.wasm"

(
  cd "${ASSET_DIR}"
  shasum -a 256 read_guest.wasm edit_guest.wasm > SHA256SUMS
)

echo "Built wasm guest artifacts in ${ASSET_DIR}"
