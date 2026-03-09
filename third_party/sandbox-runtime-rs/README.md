# sandbox-runtime-rs

OS-level sandboxing tool for enforcing filesystem and network restrictions on arbitrary processes without containerization.

## Features

- **Network Isolation**: Proxy-based domain filtering with allowlist/denylist support
- **Filesystem Restrictions**: Deny-only read access, allow-only write access patterns
- **Unix Socket Control**: Platform-specific Unix socket restrictions
- **Violation Monitoring**: Real-time tracking of sandbox policy violations
- **Cross-Platform**: Native support for macOS and Linux

## Platform Support

| Platform | Sandboxing Mechanism | Network Isolation |
|----------|---------------------|-------------------|
| macOS | Seatbelt (`sandbox-exec`) | HTTP/SOCKS5 proxy |
| Linux | Bubblewrap + seccomp | HTTP/SOCKS5 proxy + socat bridges |

## Installation

### Quick Install

Install the latest release with a single command:

```bash
curl -fsSL https://raw.githubusercontent.com/wangyedev/sandbox-runtime-rs/main/install.sh | sh
```

This will:
- Auto-detect your OS (Linux/macOS) and architecture (x86_64/arm64)
- Download the latest release from GitHub
- Install to `/usr/local/bin` (or `~/.local/bin` if sudo is unavailable)

### Building from Source

```bash
# Clone the repository
git clone https://github.com/wangyedev/sandbox-runtime-rs.git
cd sandbox-runtime-rs

# Build in release mode
cargo build --release

# The binary will be at target/release/srt
```

### Installing the Binary

```bash
# Install to ~/.cargo/bin
cargo install --path .

# Or copy manually
cp target/release/srt /usr/local/bin/
```

### Dependencies

**macOS**: No external dependencies (uses built-in `sandbox-exec`)

**Linux**:
- `bubblewrap` (bwrap) - Required for filesystem sandboxing
- `socat` - Required for network proxy bridging
- `ripgrep` (rg) - Recommended for dangerous file detection

```bash
# Debian/Ubuntu
sudo apt install bubblewrap socat ripgrep

# Fedora/RHEL
sudo dnf install bubblewrap socat ripgrep

# Arch Linux
sudo pacman -S bubblewrap socat ripgrep
```

## Quick Start

### Basic Usage

Run a command with network restrictions:

```bash
# Allow only github.com and npmjs.org
srt -c 'curl https://api.github.com/zen'
```

Run with a custom settings file:

```bash
srt -s ~/.my-sandbox-settings.json -c 'npm install'
```

Run with debug logging:

```bash
srt -d -c 'python script.py'
```

### Command-Line Options

```
srt [OPTIONS] [COMMAND]...

Options:
  -d, --debug              Enable debug logging
  -s, --settings <PATH>    Path to settings file (default: ~/.srt-settings.json)
  -c <COMMAND>             Run command string directly (sh -c mode)
  --control-fd <FD>        Read config updates from file descriptor (JSON lines protocol)
  -h, --help               Print help
  -V, --version            Print version

Arguments:
  [COMMAND]...             Command and arguments to run
```

### Examples

```bash
# Run a command with default settings
srt ls -la

# Run a shell command
srt -c 'echo $PATH && pwd'

# Run with custom settings
srt -s /path/to/settings.json npm install

# Debug mode to see what's happening
srt -d -c 'curl https://example.com'
```

## Configuration

Configuration is loaded from `~/.srt-settings.json` by default. Use the `-s` flag to specify a custom path.

### Full Configuration Schema

```json
{
  "network": {
    "allowedDomains": ["github.com", "*.npmjs.org"],
    "deniedDomains": ["evil.com"],
    "allowUnixSockets": ["/var/run/docker.sock"],
    "allowAllUnixSockets": false,
    "allowLocalBinding": true,
    "httpProxyPort": 3128,
    "socksProxyPort": 1080,
    "mitmProxy": {
      "socketPath": "/tmp/mitm.sock",
      "domains": ["api.example.com"]
    }
  },
  "filesystem": {
    "denyRead": ["/etc/shadow", "/private/var/root"],
    "allowWrite": ["/tmp", "./build", "./node_modules"],
    "denyWrite": ["/tmp/secret"],
    "allowGitConfig": false
  },
  "ignoreViolations": {
    "git": ["file-read-data.*\\.git"]
  },
  "enableWeakerNestedSandbox": false,
  "ripgrep": {
    "command": "rg",
    "args": ["--hidden"]
  },
  "mandatoryDenySearchDepth": 3,
  "allowPty": false,
  "seccomp": {
    "bpfPath": "/path/to/filter.bpf",
    "applyPath": "/path/to/apply-seccomp"
  }
}
```

### Configuration Options

#### Network Configuration (`network`)

| Option | Type | Description |
|--------|------|-------------|
| `allowedDomains` | `string[]` | Domains allowed for network access. Supports wildcards (`*.example.com`). |
| `deniedDomains` | `string[]` | Domains explicitly denied. Takes precedence over `allowedDomains`. |
| `allowLocalBinding` | `boolean` | Allow binding to localhost ports. Default: `false`. |
| `httpProxyPort` | `number` | External HTTP proxy port (if using external proxy). |
| `socksProxyPort` | `number` | External SOCKS5 proxy port (if using external proxy). |
| `mitmProxy` | `object` | MITM proxy configuration for traffic inspection. |

**Unix Socket Settings** (platform-specific behavior):

| Setting | macOS | Linux |
|---------|-------|-------|
| `allowUnixSockets: string[]` | Allowlist of socket paths | *Ignored* (seccomp can't filter by path) |
| `allowAllUnixSockets: boolean` | Allow all sockets | Disable seccomp blocking |

Unix sockets are **blocked by default** on both platforms.

- **macOS**: Use `allowUnixSockets` to allow specific paths (e.g., `["/var/run/docker.sock"]`), or `allowAllUnixSockets: true` to allow all.
- **Linux**: Blocking uses seccomp filters (x64/arm64 only). If seccomp isn't available, sockets are unrestricted and a warning is shown. Use `allowAllUnixSockets: true` to explicitly disable blocking.

#### Filesystem Configuration (`filesystem`)

| Option | Type | Description |
|--------|------|-------------|
| `denyRead` | `string[]` | Paths/patterns denied for reading. Supports globs. |
| `allowWrite` | `string[]` | Paths allowed for writing. Default: deny all writes. |
| `denyWrite` | `string[]` | Paths denied for writing. Overrides `allowWrite`. |
| `allowGitConfig` | `boolean` | Allow writes to `.git/config`. Default: `false`. |

#### Other Options

| Option | Type | Description |
|--------|------|-------------|
| `ignoreViolations` | `object` | Map of command patterns to violation regexes to ignore. |
| `enableWeakerNestedSandbox` | `boolean` | Enable weaker nested sandbox mode. |
| `ripgrep` | `object` | Ripgrep configuration for dangerous file discovery. |
| `mandatoryDenySearchDepth` | `number` | Search depth for mandatory deny discovery (Linux). Default: `3`. |
| `allowPty` | `boolean` | Allow pseudo-terminal access (macOS only). Default: `false`. |
| `seccomp` | `object` | Custom seccomp filter configuration (Linux only). |

### Example Configurations

**Development Environment**:

```json
{
  "network": {
    "allowedDomains": [
      "github.com",
      "*.github.com",
      "*.npmjs.org",
      "registry.yarnpkg.com",
      "pypi.org",
      "*.pypi.org"
    ],
    "allowLocalBinding": true
  },
  "filesystem": {
    "allowWrite": [
      "./",
      "/tmp"
    ]
  }
}
```

**Restrictive Production**:

```json
{
  "network": {
    "allowedDomains": ["api.myservice.com"],
    "deniedDomains": ["*.internal.myservice.com"]
  },
  "filesystem": {
    "denyRead": ["/etc/passwd", "/etc/shadow"],
    "allowWrite": ["/var/log/myapp"]
  }
}
```

## Library Usage

Use `sandbox-runtime` as a Rust library in your project:

```toml
[dependencies]
sandbox-runtime = { path = "../sandbox-runtime-rs" }
tokio = { version = "1", features = ["full"] }
```

```rust
use sandbox_runtime::prelude::*;
use sandbox_runtime::config::{NetworkConfig, FilesystemConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create configuration
    let config = SandboxRuntimeConfig {
        network: NetworkConfig {
            allowed_domains: vec!["github.com".to_string()],
            ..Default::default()
        },
        filesystem: FilesystemConfig {
            allow_write: vec!["/tmp".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    // Initialize the sandbox manager
    let manager = SandboxManager::new();
    manager.initialize(config).await?;

    // Wrap a command with sandbox restrictions
    let wrapped_cmd = manager.wrap_with_sandbox("curl https://api.github.com", None, None).await?;

    println!("Wrapped command: {}", wrapped_cmd);

    // Execute and cleanup
    // ... execute wrapped_cmd ...

    manager.reset().await;
    Ok(())
}
```

### Key Types

- `SandboxManager` - Main entry point for sandbox operations
- `SandboxRuntimeConfig` - Complete configuration structure
- `NetworkConfig` - Network restriction settings
- `FilesystemConfig` - Filesystem restriction settings
- `SandboxViolationStore` - In-memory violation tracking

## Architecture

```
sandbox-runtime-rs/
├── src/
│   ├── main.rs              # CLI entry point
│   ├── lib.rs               # Library exports
│   ├── cli.rs               # Command-line argument parsing
│   ├── error.rs             # Error types
│   ├── config/              # Configuration handling
│   │   ├── mod.rs
│   │   ├── schema.rs        # Config types and validation
│   │   └── loader.rs        # File loading
│   ├── manager/             # Sandbox orchestration
│   │   ├── mod.rs           # SandboxManager
│   │   ├── state.rs         # Internal state
│   │   ├── network.rs       # Proxy initialization
│   │   └── filesystem.rs    # FS config processing
│   ├── proxy/               # Network proxy servers
│   │   ├── mod.rs
│   │   ├── filter.rs        # Domain filtering logic
│   │   ├── http.rs          # HTTP/HTTPS proxy
│   │   └── socks5.rs        # SOCKS5 proxy
│   ├── sandbox/             # Platform-specific sandboxing
│   │   ├── mod.rs
│   │   ├── macos/           # macOS Seatbelt implementation
│   │   │   ├── mod.rs
│   │   │   ├── profile.rs   # Seatbelt profile generation
│   │   │   ├── wrapper.rs   # Command wrapping
│   │   │   ├── glob.rs      # Glob-to-regex conversion
│   │   │   └── monitor.rs   # Log monitoring
│   │   └── linux/           # Linux bubblewrap implementation
│   │       ├── mod.rs
│   │       ├── bwrap.rs     # Bubblewrap command generation
│   │       ├── filesystem.rs # Bind mount generation
│   │       ├── bridge.rs    # Socat bridge management
│   │       └── seccomp.rs   # Seccomp filter handling
│   ├── utils/               # Utility functions
│   │   ├── mod.rs
│   │   ├── platform.rs      # Platform detection
│   │   ├── path.rs          # Path normalization
│   │   ├── shell.rs         # Shell quoting
│   │   ├── ripgrep.rs       # Ripgrep integration
│   │   └── debug.rs         # Debug logging
│   └── violation/           # Violation tracking
│       ├── mod.rs
│       └── store.rs         # In-memory violation store
└── Cargo.toml
```

## How It Works

### Network Isolation

1. **Proxy-Based Filtering**: The sandbox starts HTTP and SOCKS5 proxy servers on localhost
2. **Environment Variables**: Commands run with `http_proxy`, `https_proxy`, and `ALL_PROXY` set
3. **Domain Filtering**: Each connection is checked against allowed/denied domain lists
4. **MITM Support**: Optional routing through a MITM proxy for inspection

```
┌─────────────────────────────────────────────────────────────┐
│                     Sandboxed Process                        │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  curl https://api.github.com                            │ │
│  └───────────────────────┬─────────────────────────────────┘ │
│                          │ HTTP_PROXY=localhost:3128         │
└──────────────────────────┼───────────────────────────────────┘
                           │
                           ▼
            ┌──────────────────────────────┐
            │       HTTP/SOCKS5 Proxy      │
            │  ┌────────────────────────┐  │
            │  │    Domain Filter       │  │
            │  │  ┌──────────────────┐  │  │
            │  │  │ allowed: ✓      │  │  │
            │  │  │ denied: ✗       │  │  │
            │  │  │ mitm: → MITM    │  │  │
            │  │  └──────────────────┘  │  │
            │  └────────────────────────┘  │
            └──────────────────────────────┘
                           │
                           ▼ (if allowed)
                    Internet
```

### Filesystem Isolation

**macOS (Seatbelt)**:
- Generates a Seatbelt profile (`.sb` file) with SBPL rules
- Uses `sandbox-exec -f profile.sb command` to run
- Supports glob patterns for path matching

**Linux (Bubblewrap)**:
- Creates isolated filesystem namespace with `bwrap`
- Mounts root as read-only, overlays writable paths
- Uses seccomp to block unauthorized Unix socket creation

### Mandatory Deny Paths

The following files/directories are always protected from writes:

**Dangerous Files**:
- `.gitconfig`, `.bashrc`, `.bash_profile`, `.profile`
- `.zshrc`, `.zprofile`, `.zshenv`, `.zlogin`
- `.npmrc`, `.yarnrc`, `.yarnrc.yml`
- `.mcp.json`, `.mcp-settings.json`

**Dangerous Directories**:
- `.git/hooks`, `.git`
- `.vscode`, `.idea`
- `.claude/commands`

## Security Considerations

### Limitations

1. **Proxy Bypass**: Sandboxed processes that don't respect proxy environment variables may bypass network filtering
2. **Root Access**: The sandbox cannot protect against processes running as root
3. **Kernel Exploits**: Sandbox escapes via kernel vulnerabilities are possible
4. **Unix Sockets (Linux)**: Without seccomp, processes may create Unix sockets to bypass network restrictions

### Best Practices

1. Always specify an explicit `allowedDomains` list rather than relying on `deniedDomains` alone
2. Use the most restrictive `allowWrite` paths possible
3. Enable seccomp on Linux when available
4. Review violation logs regularly
5. Keep the sandbox runtime updated

## Development

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run -- -c 'echo hello'
```

### Testing

```bash
# Run all tests
cargo test

# Run specific test module
cargo test config::

# Run with output
cargo test -- --nocapture
```

### Code Structure

- Platform-specific code uses `#[cfg(target_os = "...")]` attributes
- Configuration uses `serde` for JSON serialization
- Async runtime is `tokio`
- Error handling uses `thiserror` and `anyhow`

## Acknowledgments

This project is a Rust port of [sandbox-runtime](https://github.com/anthropic-experimental/sandbox-runtime), the original TypeScript implementation by Anthropic. The core architecture, sandboxing approach, and configuration schema are derived from that project.

## License

MIT License - see [LICENSE](LICENSE) for details.
