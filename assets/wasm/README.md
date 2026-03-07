# Wasm Tool Artifacts

This directory stores runtime Wasm artifacts used by `sandbox: on` tool execution.

## Required artifacts

- `read_guest.wasm`: sandboxed backend for the `read` tool.
- `edit_guest.wasm`: sandboxed backend for the `edit` tool.

## Build

```bash
./scripts/build-wasm-guests.sh
```

## Checksums

`SHA256SUMS` contains pinned checksums for all required artifacts.
