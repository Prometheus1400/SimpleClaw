//! Sandbox Runtime - OS-level sandboxing for enforcing filesystem and network restrictions.
//!
//! This library provides sandboxing capabilities for arbitrary processes without containerization:
//! - macOS: Uses Seatbelt/sandbox-exec
//! - Linux: Uses bubblewrap + seccomp

pub mod cli;
pub mod config;
pub mod error;
pub mod manager;
pub mod proxy;
pub mod sandbox;
pub mod utils;
pub mod violation;

pub use config::{
    FilesystemConfig, MitmProxyConfig, NetworkConfig, RipgrepConfig, SandboxRuntimeConfig,
    SeccompConfig,
};
pub use error::{ConfigError, Result, SandboxError};
pub use manager::SandboxManager;
pub use violation::{SandboxViolationEvent, SandboxViolationStore};

/// Re-export commonly used items.
pub mod prelude {
    pub use crate::config::SandboxRuntimeConfig;
    pub use crate::error::{Result, SandboxError};
    pub use crate::manager::SandboxManager;
    pub use crate::violation::{SandboxViolationEvent, SandboxViolationStore};
}
