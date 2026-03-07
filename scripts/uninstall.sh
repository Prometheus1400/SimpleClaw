#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.cargo}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
WASM_DIR="${WASM_DIR:-$PREFIX/share/simpleclaw/wasm}"
BIN_PATH="${BIN_DIR}/simpleclaw"
PID_PATH="${HOME}/.simpleclaw/run/service.pid"
STOP_FIRST=0

if [[ "${1:-}" == "--stop" ]]; then
  STOP_FIRST=1
elif [[ $# -gt 0 ]]; then
  echo "Usage: $0 [--stop]"
  exit 2
fi

echo "Uninstalling SimpleClaw"
echo "  prefix: ${PREFIX}"
echo "  binary: ${BIN_PATH}"
echo "  wasm dir: ${WASM_DIR}"
echo "  pid path: ${PID_PATH}"

if [[ -f "${PID_PATH}" ]]; then
  pid="$(tr -d '[:space:]' < "${PID_PATH}")"
  if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
    if [[ ${STOP_FIRST} -eq 1 ]]; then
      if [[ -x "${BIN_PATH}" ]]; then
        "${BIN_PATH}" system stop
      elif command -v simpleclaw >/dev/null 2>&1; then
        simpleclaw system stop
      else
        echo "Service is running (pid ${pid}), but no simpleclaw binary is available to stop it."
        echo "Stop it manually, then retry uninstall."
        exit 1
      fi
      sleep 1
      if kill -0 "${pid}" 2>/dev/null; then
        echo "Service is still running (pid ${pid}). Uninstall aborted."
        echo "Run 'simpleclaw system stop' and retry."
        exit 1
      fi
    else
      echo "Service is running (pid ${pid}). Uninstall aborted."
      echo "Run 'simpleclaw system stop' first, or rerun with '--stop'."
      exit 1
    fi
  fi
fi

if [[ -f "${BIN_PATH}" ]]; then
  rm -f "${BIN_PATH}"
  echo "Removed binary: ${BIN_PATH}"
else
  echo "Binary not found: ${BIN_PATH}"
fi

if [[ -d "${WASM_DIR}" ]]; then
  rm -f "${WASM_DIR}/read_tool.wasm" "${WASM_DIR}/edit_tool.wasm" "${WASM_DIR}/SHA256SUMS"
  rmdir "${WASM_DIR}" 2>/dev/null || true
  parent_dir="$(dirname "${WASM_DIR}")"
  rmdir "${parent_dir}" 2>/dev/null || true
  echo "Removed wasm artifacts from: ${WASM_DIR}"
else
  echo "Wasm dir not found: ${WASM_DIR}"
fi
