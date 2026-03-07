#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.cargo}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
WASM_DIR="${WASM_DIR:-$PREFIX/share/simpleclaw/wasm}"
MAIN_TARGET_DIR="${MAIN_TARGET_DIR:-$ROOT_DIR/target/install-main}"
WASM_TARGET_DIR="${WASM_TARGET_DIR:-$ROOT_DIR/target/install-wasm}"

echo "Installing SimpleClaw"
echo "  prefix: ${PREFIX}"
echo "  bin dir: ${BIN_DIR}"
echo "  wasm dir: ${WASM_DIR}"

mkdir -p "${BIN_DIR}" "${WASM_DIR}" "${MAIN_TARGET_DIR}" "${WASM_TARGET_DIR}"

echo "Building main binary..."
cargo build \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package simpleclaw \
  --release \
  --locked \
  --target-dir "${MAIN_TARGET_DIR}"

echo "Building wasm guests..."
cargo build \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package read_guest \
  --package edit_guest \
  --target wasm32-wasip1 \
  --release \
  --target-dir "${WASM_TARGET_DIR}"

install -m 0755 "${MAIN_TARGET_DIR}/release/simpleclaw" "${BIN_DIR}/simpleclaw"
install -m 0644 "${WASM_TARGET_DIR}/wasm32-wasip1/release/read_guest.wasm" "${WASM_DIR}/read_guest.wasm"
install -m 0644 "${WASM_TARGET_DIR}/wasm32-wasip1/release/edit_guest.wasm" "${WASM_DIR}/edit_guest.wasm"
(
  cd "${WASM_DIR}"
  shasum -a 256 read_guest.wasm edit_guest.wasm > SHA256SUMS
)

echo
echo "Installed binary:   ${BIN_DIR}/simpleclaw"
echo "Installed wasm:     ${WASM_DIR}"

if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
  echo
  echo "Add this to your shell profile to use simpleclaw directly:"
  echo "  export PATH=\"${BIN_DIR}:\$PATH\""
fi
