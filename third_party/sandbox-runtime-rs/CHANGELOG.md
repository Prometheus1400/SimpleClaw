# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-01-24

### Fixed

- **Control-FD Safety**: Added validation to reject negative file descriptor values with a clear error message
- **WSL Detection**: Fixed case sensitivity bug in WSL version parsing that could cause incorrect byte position indexing
- **Mutex Poisoning**: Cache lookups now recover gracefully if another thread panicked while holding the lock

### Changed

- **Control-FD Handling**: Refactored to use `tokio::select!` with a shutdown channel for graceful task termination
- **Platform Code**: Improved unused variable suppression to only apply on non-Linux platforms using `cfg_attr`
- **Error Logging**: `load_config_from_string()` now logs parsing failures at debug level for better debugging

### Added

- Unit tests for `load_config_from_string()` covering valid JSON, empty strings, invalid JSON, and whitespace handling
- Unit tests for WSL version parsing covering WSL1, WSL2, native Linux, and forward compatibility with future versions
- Documentation for WSL detection explaining the logic and WSL1/WSL2 differences

## [0.1.0] - 2026-01-23

### Added

- Initial release
- OS-level sandboxing for macOS (Seatbelt) and Linux (bubblewrap + seccomp)
- HTTP and SOCKS5 proxy-based network filtering
- Domain allowlist/denylist with wildcard pattern support
- MITM proxy routing for specific domains
- Filesystem read/write restrictions with glob pattern support
- Dynamic configuration updates via control file descriptor
- Mandatory deny paths for security-sensitive files
