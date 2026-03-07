#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.cargo}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
WASM_DIR="${WASM_DIR:-$PREFIX/share/simpleclaw/wasm}"
MAIN_TARGET_DIR="${MAIN_TARGET_DIR:-$ROOT_DIR/target/install-main}"
WASM_TARGET_DIR="${WASM_TARGET_DIR:-$ROOT_DIR/target/install-wasm}"
BUILD_PROFILE="release"
CARGO_PROFILE_ARGS=(--release)

usage() {
  cat <<EOF
Usage: $0 [--debug]

Options:
  --debug   Build and install debug artifacts instead of release.
  -h, --help
EOF
}

while (($# > 0)); do
  case "$1" in
    --debug)
      BUILD_PROFILE="debug"
      CARGO_PROFILE_ARGS=()
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

echo "Installing SimpleClaw"
echo "  prefix: ${PREFIX}"
echo "  bin dir: ${BIN_DIR}"
echo "  wasm dir: ${WASM_DIR}"
echo "  profile: ${BUILD_PROFILE}"

mkdir -p "${BIN_DIR}" "${WASM_DIR}" "${MAIN_TARGET_DIR}" "${WASM_TARGET_DIR}"

echo "Building main binary..."
cargo build \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package simpleclaw \
  "${CARGO_PROFILE_ARGS[@]}" \
  --locked \
  --target-dir "${MAIN_TARGET_DIR}"

echo "Building wasm tools..."
cargo build \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package read_tool \
  --package edit_tool \
  --target wasm32-wasip1 \
  "${CARGO_PROFILE_ARGS[@]}" \
  --target-dir "${WASM_TARGET_DIR}"

install -m 0755 "${MAIN_TARGET_DIR}/${BUILD_PROFILE}/simpleclaw" "${BIN_DIR}/simpleclaw"
install -m 0644 "${WASM_TARGET_DIR}/wasm32-wasip1/${BUILD_PROFILE}/read_tool.wasm" "${WASM_DIR}/read_tool.wasm"
install -m 0644 "${WASM_TARGET_DIR}/wasm32-wasip1/${BUILD_PROFILE}/edit_tool.wasm" "${WASM_DIR}/edit_tool.wasm"
(
  cd "${WASM_DIR}"
  shasum -a 256 read_tool.wasm edit_tool.wasm > SHA256SUMS
)

echo
echo "Installed binary:   ${BIN_DIR}/simpleclaw"
echo "Installed wasm:     ${WASM_DIR}"

if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
  echo
  echo "Add this to your shell profile to use simpleclaw directly:"
  echo "  export PATH=\"${BIN_DIR}:\$PATH\""
fi
