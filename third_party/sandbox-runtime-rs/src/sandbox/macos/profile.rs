//! Seatbelt profile generation for macOS sandbox.

use std::collections::HashSet;

use crate::config::{
    FilesystemConfig, NetworkConfig, SandboxRuntimeConfig, DANGEROUS_DIRECTORIES, DANGEROUS_FILES,
};
use crate::sandbox::macos::glob::glob_to_seatbelt_regex;
use crate::utils::{normalize_path_for_sandbox, contains_glob_chars};

/// Session suffix for log tagging (generated once per session).
static SESSION_SUFFIX: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    format!("{:08x}", rng.gen::<u32>())
});

/// Generate a unique log tag for a command.
pub fn generate_log_tag(command: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(command);
    format!("CMD64_{}_END_{}", encoded, *SESSION_SUFFIX)
}

/// Generate a Seatbelt profile for the given configuration.
pub fn generate_profile(
    config: &SandboxRuntimeConfig,
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
    log_tag: Option<&str>,
) -> String {
    let mut profile = String::new();

    // Version and deny default
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n\n");

    // Add log tag if provided (for violation monitoring)
    if let Some(tag) = log_tag {
        profile.push_str(&format!("; Log tag: {}\n", tag));
        profile.push_str(&format!("(trace \"{tag}\")\n\n"));
    }

    // Process rules
    profile.push_str("; Process\n");
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow process-info*)\n");
    profile.push_str("(allow process-codesigning-status*)\n\n");

    // Signal rules
    profile.push_str("; Signals\n");
    profile.push_str("(allow signal)\n\n");

    // Sysctl rules
    profile.push_str("; Sysctl\n");
    profile.push_str("(allow sysctl-read)\n\n");

    // Mach rules
    profile.push_str("; Mach\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow mach-register)\n\n");

    // IPC rules
    profile.push_str("; IPC\n");
    profile.push_str("(allow ipc-posix*)\n");
    profile.push_str("(allow ipc-sysv*)\n\n");

    // PTY support
    if config.allow_pty.unwrap_or(false) {
        profile.push_str("; PTY\n");
        profile.push_str("(allow pseudo-tty)\n");
        profile.push_str("(allow file-ioctl (regex #\"^/dev/ttys\"))\n\n");
    }

    // Network rules
    profile.push_str("; Network\n");
    generate_network_rules(&mut profile, &config.network, http_proxy_port, socks_proxy_port);
    profile.push('\n');

    // Filesystem rules
    profile.push_str("; Filesystem\n");
    generate_filesystem_rules(&mut profile, &config.filesystem);

    profile
}

/// Generate network rules for the Seatbelt profile.
fn generate_network_rules(
    profile: &mut String,
    config: &NetworkConfig,
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
) {
    // If no network restrictions (empty allowed domains), allow all network
    if config.allowed_domains.is_empty() && config.denied_domains.is_empty() {
        profile.push_str("(allow network*)\n");
        return;
    }

    // Allow localhost connections to proxy ports
    if let Some(port) = http_proxy_port {
        profile.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{}\"))\n",
            port
        ));
    }
    if let Some(port) = socks_proxy_port {
        profile.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{}\"))\n",
            port
        ));
    }

    // Allow local binding if configured
    if config.allow_local_binding.unwrap_or(false) {
        profile.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
    }

    // Allow specific Unix sockets
    if let Some(ref sockets) = config.allow_unix_sockets {
        for socket in sockets {
            let normalized = normalize_path_for_sandbox(socket);
            profile.push_str(&format!(
                "(allow network* (subpath \"{}\"))\n",
                escape_seatbelt_string(&normalized)
            ));
        }
    }

    // Allow DNS lookups
    profile.push_str("(allow network-outbound (remote ip \"*:53\"))\n");
    profile.push_str("(allow network-outbound (remote ip \"*:853\"))\n");
}

/// Generate filesystem rules for the Seatbelt profile.
fn generate_filesystem_rules(profile: &mut String, config: &FilesystemConfig) {
    // Read rules: allow all, then deny specific paths
    profile.push_str("; Read access (deny-only pattern)\n");
    profile.push_str("(allow file-read*)\n");

    // Deny read for specific paths
    for path in &config.deny_read {
        let normalized = normalize_path_for_sandbox(path);
        if contains_glob_chars(&normalized) {
            let regex = glob_to_seatbelt_regex(&normalized);
            profile.push_str(&format!("(deny file-read* (regex #\"{}\"))\n", regex));
        } else {
            profile.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                escape_seatbelt_string(&normalized)
            ));
        }
    }

    profile.push('\n');

    // Write rules: deny all, then allow specific paths
    profile.push_str("; Write access (allow-only pattern)\n");

    // Collect all allowed write paths
    let mut allowed_paths: HashSet<String> = HashSet::new();
    for path in &config.allow_write {
        let normalized = normalize_path_for_sandbox(path);
        allowed_paths.insert(normalized);
    }

    // Generate allow rules for each path
    for path in &allowed_paths {
        if contains_glob_chars(path) {
            let regex = glob_to_seatbelt_regex(path);
            profile.push_str(&format!("(allow file-write* (regex #\"{}\"))\n", regex));
        } else {
            profile.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_seatbelt_string(path)
            ));
        }
    }

    // Deny write for specific paths (overrides allow)
    for path in &config.deny_write {
        let normalized = normalize_path_for_sandbox(path);
        if contains_glob_chars(&normalized) {
            let regex = glob_to_seatbelt_regex(&normalized);
            profile.push_str(&format!("(deny file-write* (regex #\"{}\"))\n", regex));
        } else {
            profile.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                escape_seatbelt_string(&normalized)
            ));
        }
    }

    // Add mandatory deny rules for dangerous files/directories
    profile.push_str("\n; Mandatory deny (dangerous files)\n");
    generate_mandatory_deny_rules(profile, config);

    // Deny moves/renames to prevent circumventing write restrictions
    profile.push_str("\n; Block file moves/renames\n");
    profile.push_str("(deny file-write-unlink)\n");
}

/// Generate mandatory deny rules for dangerous files and directories.
fn generate_mandatory_deny_rules(profile: &mut String, config: &FilesystemConfig) {
    // Deny dangerous files (case-insensitive)
    for file in DANGEROUS_FILES {
        // Skip .gitconfig if allowGitConfig is true
        if *file == ".gitconfig" && config.allow_git_config.unwrap_or(false) {
            continue;
        }

        // Use regex for case-insensitive matching
        let regex = format!(
            "^.*/{}$",
            file.chars()
                .map(|c| {
                    if c.is_ascii_alphabetic() {
                        format!("[{}{}]", c.to_ascii_uppercase(), c.to_ascii_lowercase())
                    } else if c == '.' {
                        "\\.".to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        );
        profile.push_str(&format!("(deny file-write* (regex #\"{}\"))\n", regex));
    }

    // Deny dangerous directories
    for dir in DANGEROUS_DIRECTORIES {
        // Skip .git/config if allowGitConfig is true
        if *dir == ".git" && config.allow_git_config.unwrap_or(false) {
            // Only block .git/hooks, not all of .git
            profile.push_str("(deny file-write* (subpath \"/.git/hooks\"))\n");
            continue;
        }

        profile.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", dir));
        // Also match the pattern anywhere in the path
        profile.push_str(&format!(
            "(deny file-write* (regex #\"^.*/{}(/.*)?$\"))\n",
            regex::escape(dir)
        ));
    }
}

/// Escape a string for use in a Seatbelt profile.
fn escape_seatbelt_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_log_tag() {
        let tag = generate_log_tag("echo hello");
        assert!(tag.starts_with("CMD64_"));
        assert!(tag.contains("_END_"));
    }

    #[test]
    fn test_escape_seatbelt_string() {
        assert_eq!(escape_seatbelt_string("simple"), "simple");
        assert_eq!(escape_seatbelt_string("with\\slash"), "with\\\\slash");
        assert_eq!(escape_seatbelt_string("with\"quote"), "with\\\"quote");
    }

    #[test]
    fn test_generate_profile_minimal() {
        let config = SandboxRuntimeConfig::default();
        let profile = generate_profile(&config, None, None, None);

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(allow file-read*)"));
    }

    #[test]
    fn test_generate_profile_with_network() {
        let config = SandboxRuntimeConfig {
            network: NetworkConfig {
                allowed_domains: vec!["github.com".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        let profile = generate_profile(&config, Some(3128), Some(1080), None);

        assert!(profile.contains("localhost:3128"));
        assert!(profile.contains("localhost:1080"));
    }

    #[test]
    fn test_generate_profile_with_pty() {
        let config = SandboxRuntimeConfig {
            allow_pty: Some(true),
            ..Default::default()
        };
        let profile = generate_profile(&config, None, None, None);

        assert!(profile.contains("(allow pseudo-tty)"));
    }
}
