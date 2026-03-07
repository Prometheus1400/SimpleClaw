# SimpleClaw

SimpleClaw is a Rust project with sandboxed WASM guest artifacts used for tool execution.
The repository is organized as a Cargo workspace, with the main app at the root and wasm guest crates under `guests/`.

## Common Commands

- `cargo build`: Build the main Rust project.
- `cargo run`: Run the main Rust project.
- `cargo check`: Run type and borrow checks.
- `cargo test`: Run tests.
- `cargo fmt --all`: Format all Rust code.

## WASM Artifacts

WASM guest artifacts live in `assets/wasm`.

Build and stage wasm artifacts into `assets/wasm`:

```bash
./scripts/build-wasm-guests.sh
```

Or build wasm guests from the root workspace manifest directly:

```bash
cargo build --package read_guest --package edit_guest --target wasm32-wasip1 --release
```

## Install

Install locally (default prefix `~/.cargo`):

```bash
./scripts/install.sh
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
