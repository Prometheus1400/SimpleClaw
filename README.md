# SimpleClaw

SimpleClaw is a Rust project with sandboxed WASM tool artifacts used for tool execution.
The repository is organized as a Cargo workspace, with the main app at the root and wasm tool crates under `sandbox/`.

## Common Commands

- `cargo build`: Build the main Rust project.
- `cargo run`: Run the main Rust project.
- `cargo check`: Run type and borrow checks.
- `cargo test`: Run tests.
- `cargo fmt --all`: Format all Rust code.

## WASM Artifacts

WASM guest artifacts live in `assets/wasm`.

Build wasm artifacts from the root workspace manifest:

```bash
cargo build --package read_tool --package edit_tool --target wasm32-wasip1 --release
```

## Install

Install locally (default prefix `~/.cargo`):

```bash
./scripts/install.sh
```

Install debug artifacts (faster for local development):

```bash
./scripts/install.sh --debug
```

Or call the script directly with an override:

```bash
PREFIX="$HOME/.cargo" ./scripts/install.sh
```

The installer places:
- binary: `~/.cargo/bin/simpleclaw`
- wasm assets: `~/.cargo/share/simpleclaw/wasm`

At runtime, wasm tools are resolved from `SIMPLECLAW_WASM_ASSETS_DIR` if set, otherwise from `<binary-prefix>/share/simpleclaw/wasm` (for example `~/.cargo/share/simpleclaw/wasm`).

## Uninstall

Remove the installed binary and wasm artifacts (default prefix `~/.cargo`):

```bash
./scripts/uninstall.sh
```

If the service is running, stop and uninstall in one step:

```bash
./scripts/uninstall.sh --stop
```
