//! Configuration module.

pub mod loader;
pub mod schema;

pub use loader::{default_settings_path, load_config, load_config_from_string, load_default_config, parse_config};
pub use schema::{
    matches_domain_pattern, FilesystemConfig, MitmProxyConfig, NetworkConfig, RipgrepConfig,
    SandboxRuntimeConfig, SeccompConfig, DANGEROUS_DIRECTORIES, DANGEROUS_FILES,
};
