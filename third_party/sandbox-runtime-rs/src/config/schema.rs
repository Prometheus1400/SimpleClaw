//! Configuration schema types matching the TypeScript Zod schemas.

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, SandboxError};

/// MITM proxy configuration for routing specific domains through a man-in-the-middle proxy.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MitmProxyConfig {
    /// Unix socket path for the MITM proxy.
    pub socket_path: String,
    /// Domains to route through the MITM proxy.
    pub domains: Vec<String>,
}

/// Network restriction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    /// Domains allowed for network access (e.g., "github.com", "*.npmjs.org").
    #[serde(default)]
    pub allowed_domains: Vec<String>,

    /// Domains explicitly denied for network access.
    #[serde(default)]
    pub denied_domains: Vec<String>,

    /// macOS only: Unix socket paths to allow.
    /// Ignored on Linux (seccomp cannot filter by path).
    #[serde(default)]
    pub allow_unix_sockets: Option<Vec<String>>,

    /// If true, allow all Unix sockets (disables blocking on both platforms).
    /// On macOS: allows all socket paths.
    /// On Linux: disables seccomp blocking (sockets are blocked by default).
    #[serde(default)]
    pub allow_all_unix_sockets: Option<bool>,

    /// Allow binding to localhost.
    #[serde(default)]
    pub allow_local_binding: Option<bool>,

    /// External HTTP proxy port.
    #[serde(default)]
    pub http_proxy_port: Option<u16>,

    /// External SOCKS proxy port.
    #[serde(default)]
    pub socks_proxy_port: Option<u16>,

    /// MITM proxy configuration.
    #[serde(default)]
    pub mitm_proxy: Option<MitmProxyConfig>,
}

/// Filesystem restriction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemConfig {
    /// Paths/patterns denied for reading.
    #[serde(default)]
    pub deny_read: Vec<String>,

    /// Paths allowed for writing.
    #[serde(default)]
    pub allow_write: Vec<String>,

    /// Paths denied for writing (overrides allow_write).
    #[serde(default)]
    pub deny_write: Vec<String>,

    /// Allow writes to .git/config.
    #[serde(default)]
    pub allow_git_config: Option<bool>,
}

/// Ripgrep configuration for dangerous file discovery on Linux.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RipgrepConfig {
    /// Path to the ripgrep command.
    pub command: String,
    /// Additional arguments.
    #[serde(default)]
    pub args: Option<Vec<String>>,
}

impl Default for RipgrepConfig {
    fn default() -> Self {
        Self {
            command: "rg".to_string(),
            args: None,
        }
    }
}

/// Custom seccomp filter configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SeccompConfig {
    /// Path to custom BPF filter.
    pub bpf_path: Option<String>,
    /// Path to custom apply-seccomp binary.
    pub apply_path: Option<String>,
}

/// Main sandbox runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRuntimeConfig {
    /// Network restriction configuration.
    #[serde(default)]
    pub network: NetworkConfig,

    /// Filesystem restriction configuration.
    #[serde(default)]
    pub filesystem: FilesystemConfig,

    /// Violation filtering by command pattern.
    #[serde(default)]
    pub ignore_violations: Option<std::collections::HashMap<String, Vec<String>>>,

    /// Enable weaker nested sandbox mode.
    #[serde(default)]
    pub enable_weaker_nested_sandbox: Option<bool>,

    /// Ripgrep configuration.
    #[serde(default)]
    pub ripgrep: Option<RipgrepConfig>,

    /// Search depth for mandatory deny discovery (Linux, default: 3).
    #[serde(default)]
    pub mandatory_deny_search_depth: Option<u32>,

    /// Allow pseudo-terminal (macOS only).
    #[serde(default)]
    pub allow_pty: Option<bool>,

    /// Custom seccomp configuration.
    #[serde(default)]
    pub seccomp: Option<SeccompConfig>,
}

/// Dangerous files that should never be writable.
pub const DANGEROUS_FILES: &[&str] = &[
    ".gitconfig",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".mcp.json",
    ".mcp-settings.json",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
];

/// Dangerous directories that should never be writable.
pub const DANGEROUS_DIRECTORIES: &[&str] = &[
    ".git/hooks",
    ".git",
    ".vscode",
    ".idea",
    ".claude/commands",
];

impl SandboxRuntimeConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), SandboxError> {
        // Validate allowed domains
        for domain in &self.network.allowed_domains {
            validate_domain_pattern(domain)?;
        }

        // Validate denied domains
        for domain in &self.network.denied_domains {
            validate_domain_pattern(domain)?;
        }

        // Validate MITM proxy domains
        if let Some(ref mitm) = self.network.mitm_proxy {
            for domain in &mitm.domains {
                validate_domain_pattern(domain)?;
            }
        }

        Ok(())
    }
}

/// Validate a domain pattern.
fn validate_domain_pattern(pattern: &str) -> Result<(), SandboxError> {
    // Check for empty pattern
    if pattern.is_empty() {
        return Err(ConfigError::InvalidDomainPattern {
            pattern: pattern.to_string(),
            reason: "domain pattern cannot be empty".to_string(),
        }
        .into());
    }

    // Check for just wildcard
    if pattern == "*" {
        return Err(ConfigError::InvalidDomainPattern {
            pattern: pattern.to_string(),
            reason: "wildcard-only patterns are not allowed".to_string(),
        }
        .into());
    }

    // Check for too broad patterns like *.com
    if pattern.starts_with("*.") {
        let suffix = &pattern[2..];
        // Check if suffix is a TLD or too short
        if !suffix.contains('.') && suffix.len() <= 4 {
            return Err(ConfigError::InvalidDomainPattern {
                pattern: pattern.to_string(),
                reason: "pattern is too broad (matches entire TLD)".to_string(),
            }
            .into());
        }
    }

    // Check for port numbers
    if pattern.contains(':') {
        return Err(ConfigError::InvalidDomainPattern {
            pattern: pattern.to_string(),
            reason: "domain patterns cannot include port numbers".to_string(),
        }
        .into());
    }

    // Check for invalid characters
    let check_part = if pattern.starts_with("*.") {
        &pattern[2..]
    } else {
        pattern
    };

    for ch in check_part.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-' && ch != '_' {
            return Err(ConfigError::InvalidDomainPattern {
                pattern: pattern.to_string(),
                reason: format!("invalid character '{}' in domain pattern", ch),
            }
            .into());
        }
    }

    Ok(())
}

/// Check if a hostname matches a domain pattern.
pub fn matches_domain_pattern(hostname: &str, pattern: &str) -> bool {
    let hostname_lower = hostname.to_lowercase();
    let pattern_lower = pattern.to_lowercase();

    if pattern_lower.starts_with("*.") {
        // Wildcard pattern: *.example.com matches api.example.com but NOT example.com
        let base_domain = &pattern_lower[2..];
        hostname_lower.ends_with(&format!(".{}", base_domain))
    } else {
        // Exact match
        hostname_lower == pattern_lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_pattern_matching() {
        // Exact match
        assert!(matches_domain_pattern("example.com", "example.com"));
        assert!(matches_domain_pattern("EXAMPLE.COM", "example.com"));
        assert!(!matches_domain_pattern("api.example.com", "example.com"));

        // Wildcard match
        assert!(matches_domain_pattern("api.example.com", "*.example.com"));
        assert!(matches_domain_pattern("deep.api.example.com", "*.example.com"));
        assert!(!matches_domain_pattern("example.com", "*.example.com"));

        // Case insensitivity
        assert!(matches_domain_pattern("API.EXAMPLE.COM", "*.example.com"));
    }

    #[test]
    fn test_domain_pattern_validation() {
        // Valid patterns
        assert!(validate_domain_pattern("example.com").is_ok());
        assert!(validate_domain_pattern("*.example.com").is_ok());
        assert!(validate_domain_pattern("localhost").is_ok());
        assert!(validate_domain_pattern("api.github.com").is_ok());

        // Invalid patterns
        assert!(validate_domain_pattern("").is_err());
        assert!(validate_domain_pattern("*").is_err());
        assert!(validate_domain_pattern("*.com").is_err());
        assert!(validate_domain_pattern("example.com:8080").is_err());
    }
}
