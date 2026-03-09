//! Configuration loader from ~/.srt-settings.json.

use std::path::{Path, PathBuf};

use crate::config::schema::SandboxRuntimeConfig;
use crate::error::{ConfigError, SandboxError};

/// Default settings file name.
const DEFAULT_SETTINGS_FILE: &str = ".srt-settings.json";

/// Get the default settings file path.
pub fn default_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(DEFAULT_SETTINGS_FILE))
}

/// Load configuration from a file path.
pub fn load_config(path: &Path) -> Result<SandboxRuntimeConfig, SandboxError> {
    if !path.exists() {
        return Err(ConfigError::FileNotFound(path.display().to_string()).into());
    }

    let content = std::fs::read_to_string(path).map_err(|e| {
        ConfigError::ParseError(format!("Failed to read config file: {}", e))
    })?;

    parse_config(&content)
}

/// Load configuration from the default path, or return default config if not found.
pub fn load_default_config() -> Result<SandboxRuntimeConfig, SandboxError> {
    match default_settings_path() {
        Some(path) if path.exists() => load_config(&path),
        _ => Ok(SandboxRuntimeConfig::default()),
    }
}

/// Parse configuration from a JSON string.
pub fn parse_config(json: &str) -> Result<SandboxRuntimeConfig, SandboxError> {
    let config: SandboxRuntimeConfig = serde_json::from_str(json).map_err(|e| {
        ConfigError::ParseError(format!("Failed to parse config JSON: {}", e))
    })?;

    // Validate the configuration
    config.validate()?;

    Ok(config)
}

/// Load and validate sandbox configuration from a string.
/// Used for parsing config from control fd (JSON lines protocol).
/// Returns None if the string is empty, invalid JSON, or fails validation.
pub fn load_config_from_string(content: &str) -> Option<SandboxRuntimeConfig> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    match parse_config(trimmed) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::debug!("Failed to parse config from string: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{}"#;
        let config = parse_config(json).unwrap();
        assert!(config.network.allowed_domains.is_empty());
        assert!(config.filesystem.allow_write.is_empty());
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            "network": {
                "allowedDomains": ["github.com", "*.npmjs.org"],
                "deniedDomains": ["evil.com"],
                "allowLocalBinding": true,
                "mitmProxy": {
                    "socketPath": "/tmp/mitm.sock",
                    "domains": ["api.example.com"]
                }
            },
            "filesystem": {
                "denyRead": ["/etc/passwd"],
                "allowWrite": ["/tmp"],
                "denyWrite": ["/tmp/secret"],
                "allowGitConfig": false
            },
            "mandatoryDenySearchDepth": 5,
            "allowPty": true
        }"#;

        let config = parse_config(json).unwrap();
        assert_eq!(config.network.allowed_domains.len(), 2);
        assert_eq!(config.network.denied_domains.len(), 1);
        assert_eq!(config.network.allow_local_binding, Some(true));
        assert!(config.network.mitm_proxy.is_some());
        assert_eq!(config.filesystem.deny_read.len(), 1);
        assert_eq!(config.filesystem.allow_write.len(), 1);
        assert_eq!(config.filesystem.deny_write.len(), 1);
        assert_eq!(config.mandatory_deny_search_depth, Some(5));
        assert_eq!(config.allow_pty, Some(true));
    }

    #[test]
    fn test_invalid_domain_pattern() {
        let json = r#"{
            "network": {
                "allowedDomains": ["*.com"]
            }
        }"#;

        let result = parse_config(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_from_string_valid() {
        let json = r#"{"network": {"allowedDomains": ["github.com"]}}"#;
        let config = load_config_from_string(json);
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.network.allowed_domains.len(), 1);
        assert_eq!(config.network.allowed_domains[0], "github.com");
    }

    #[test]
    fn test_load_config_from_string_empty() {
        assert!(load_config_from_string("").is_none());
        assert!(load_config_from_string("   ").is_none());
        assert!(load_config_from_string("\n\t").is_none());
    }

    #[test]
    fn test_load_config_from_string_invalid_json() {
        assert!(load_config_from_string("not json").is_none());
        assert!(load_config_from_string("{invalid}").is_none());
        assert!(load_config_from_string("{\"network\": }").is_none());
    }

    #[test]
    fn test_load_config_from_string_with_whitespace() {
        let json = r#"   {"network": {"allowedDomains": ["example.com"]}}   "#;
        let config = load_config_from_string(json);
        assert!(config.is_some());
        assert_eq!(config.unwrap().network.allowed_domains[0], "example.com");
    }
}
