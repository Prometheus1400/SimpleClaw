#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.cargo}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
WASM_DIR="${WASM_DIR:-$PREFIX/share/simpleclaw/wasm}"
BIN_PATH="${BIN_DIR}/simpleclaw"

echo "Uninstalling SimpleClaw"
echo "  prefix: ${PREFIX}"
echo "  binary: ${BIN_PATH}"
echo "  wasm dir: ${WASM_DIR}"

if [[ -f "${BIN_PATH}" ]]; then
  rm -f "${BIN_PATH}"
  echo "Removed binary: ${BIN_PATH}"
else
  echo "Binary not found: ${BIN_PATH}"
fi

if [[ -d "${WASM_DIR}" ]]; then
  rm -f "${WASM_DIR}/read_guest.wasm" "${WASM_DIR}/edit_guest.wasm" "${WASM_DIR}/SHA256SUMS"
  rmdir "${WASM_DIR}" 2>/dev/null || true
  parent_dir="$(dirname "${WASM_DIR}")"
  rmdir "${parent_dir}" 2>/dev/null || true
  echo "Removed wasm artifacts from: ${WASM_DIR}"
else
  echo "Wasm dir not found: ${WASM_DIR}"
fi
