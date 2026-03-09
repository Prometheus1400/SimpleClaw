# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Upstream Reference

This project is a **Rust implementation** of [sandbox-runtime](https://github.com/anthropic-experimental/sandbox-runtime), the original TypeScript implementation by Anthropic.

**Key points:**
- The architecture, configuration schema, and sandboxing approach mirror the upstream project
- When implementing new features or fixing bugs, consult the upstream repository for design intent
- Configuration JSON format (`~/.srt-settings.json`) is designed to be compatible with the upstream schema
- Mandatory deny paths, domain filtering logic, and platform-specific sandboxing follow upstream behavior

**When making changes:**
1. Check if the feature/fix exists in the upstream TypeScript implementation
2. Align behavior with upstream unless there's a Rust-specific reason to diverge
3. Document any intentional deviations from upstream in code comments

## Build & Test Commands

```bash
cargo build                   # Debug build
cargo build --release         # Release build (binary: target/release/srt)
cargo test                    # Run all tests
cargo test config::           # Run specific module tests
cargo test -- --nocapture     # Run with output visible
cargo clippy                  # Lint
```

## Running the CLI

```bash
cargo run -- -c 'echo hello'              # Run command in sandbox
cargo run -- -d -c 'curl example.com'     # Debug mode
cargo run -- -s settings.json -c 'cmd'    # Custom settings file
```

## Architecture

OS-level sandboxing tool enforcing filesystem and network restrictions without containerization. Uses proxy-based network filtering (portable, no root required) with platform-specific sandboxing.

### Core Flow

1. `SandboxManager::initialize()` - Starts HTTP/SOCKS5 proxies, validates config
2. `SandboxManager::wrap_with_sandbox()` - Generates platform-specific wrapped command
3. Wrapped command runs with proxy env vars (`http_proxy`, `https_proxy`, `ALL_PROXY`)

### Platform Implementations

**macOS** (`src/sandbox/macos/`):
- Uses Seatbelt (`sandbox-exec`) with SBPL profiles
- `profile.rs`: generates `.sb` profile with `generate_profile()`
- Glob patterns → Seatbelt regex via `glob_to_seatbelt_regex()`

**Linux** (`src/sandbox/linux/`):
- Uses bubblewrap + seccomp
- `bwrap.rs`: generates bwrap command with `--unshare-net`, bind mounts
- `bridge.rs`: socat bridges for proxy access inside namespace

### Key Modules

| Module | Purpose |
|--------|---------|
| `manager/` | Orchestration: proxy init, command wrapping, state |
| `proxy/filter.rs` | Domain filtering: allowlist/denylist/MITM decisions |
| `config/schema.rs` | Configuration types, validation, dangerous paths |
| `sandbox/macos/profile.rs` | Seatbelt SBPL profile generation |
| `sandbox/linux/bwrap.rs` | Bubblewrap command construction |

### Domain Filter Priority (`src/proxy/filter.rs`)

1. `deniedDomains` checked first (highest priority)
2. `mitmDomains` for MITM routing
3. `allowedDomains`: if empty allow all, otherwise only matching

Pattern matching: `*.example.com` matches `api.example.com` but NOT `example.com`

### Mandatory Deny Paths (`src/config/schema.rs`)

Always write-protected: `.gitconfig`, `.bashrc`, `.zshrc`, `.npmrc`, `.mcp.json`, `.git/hooks`, `.vscode`, `.idea`, `.claude/commands`

## Code Patterns

- Platform code: `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "linux")]`
- Config JSON uses camelCase: `#[serde(rename_all = "camelCase")]`
- Async: `tokio`, Errors: `thiserror`, Logging: `tracing`, Locks: `parking_lot`

## Modifying Sandbox Behavior

**New config option:**
1. Add to struct in `src/config/schema.rs` with `#[serde(default)]`
2. Validate in `SandboxRuntimeConfig::validate()` if needed
3. Handle in `sandbox/macos/profile.rs` or `sandbox/linux/bwrap.rs`

**macOS sandbox:** Edit `src/sandbox/macos/profile.rs` (SBPL generation functions)

**Linux sandbox:** Edit `src/sandbox/linux/bwrap.rs` (command) or `filesystem.rs` (mounts)

## Debugging

```bash
cargo run -- -d -c 'command'   # Debug logging via -d flag
RUST_LOG=debug cargo run -- -c 'command'

# macOS: Watch sandbox violations
log stream --predicate 'subsystem == "com.apple.sandbox"' --debug
```
