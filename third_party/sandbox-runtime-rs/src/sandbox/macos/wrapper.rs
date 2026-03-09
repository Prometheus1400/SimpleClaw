//! Command wrapping for macOS sandbox-exec.


use crate::config::SandboxRuntimeConfig;
use crate::error::SandboxError;
use crate::sandbox::macos::profile::{generate_log_tag, generate_profile};
use crate::utils::quote;

/// Wrap a command with sandbox-exec.
pub fn wrap_command(
    command: &str,
    config: &SandboxRuntimeConfig,
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
    shell: Option<&str>,
    enable_log_monitor: bool,
) -> Result<(String, Option<String>), SandboxError> {
    let shell = shell.unwrap_or("/bin/bash");

    // Generate log tag for violation monitoring
    let log_tag = if enable_log_monitor {
        Some(generate_log_tag(command))
    } else {
        None
    };

    // Generate the Seatbelt profile
    let profile = generate_profile(config, http_proxy_port, socks_proxy_port, log_tag.as_deref());

    // Write profile to a temporary file
    let profile_path = write_profile_to_temp(&profile)?;

    // Build the wrapped command
    let wrapped = format!(
        "sandbox-exec -f {} {} -c {}",
        quote(&profile_path),
        shell,
        quote(command)
    );

    Ok((wrapped, log_tag))
}

/// Write the profile to a temporary file.
fn write_profile_to_temp(profile: &str) -> Result<String, SandboxError> {
    use std::io::Write;

    let temp_dir = std::env::temp_dir();
    let filename = format!("srt-profile-{}.sb", std::process::id());
    let path = temp_dir.join(filename);

    let mut file = std::fs::File::create(&path)?;
    file.write_all(profile.as_bytes())?;

    Ok(path.display().to_string())
}

/// Clean up temporary profile files.
pub fn cleanup_temp_profiles() {
    let temp_dir = std::env::temp_dir();
    let pattern = format!("srt-profile-{}.sb", std::process::id());
    let path = temp_dir.join(pattern);

    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Generate proxy environment variables.
pub fn generate_proxy_env(
    http_proxy_port: u16,
    socks_proxy_port: u16,
) -> Vec<(String, String)> {
    let http_proxy = format!("http://localhost:{}", http_proxy_port);
    let socks_proxy = format!("socks5://localhost:{}", socks_proxy_port);

    vec![
        ("http_proxy".to_string(), http_proxy.clone()),
        ("HTTP_PROXY".to_string(), http_proxy.clone()),
        ("https_proxy".to_string(), http_proxy.clone()),
        ("HTTPS_PROXY".to_string(), http_proxy),
        ("ALL_PROXY".to_string(), socks_proxy.clone()),
        ("all_proxy".to_string(), socks_proxy),
        // For git SSH
        (
            "GIT_SSH_COMMAND".to_string(),
            format!(
                "ssh -o ProxyCommand='nc -X 5 -x localhost:{} %h %p'",
                socks_proxy_port
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_proxy_env() {
        let env = generate_proxy_env(3128, 1080);
        assert!(env.iter().any(|(k, v)| k == "http_proxy" && v.contains("3128")));
        assert!(env.iter().any(|(k, v)| k == "ALL_PROXY" && v.contains("1080")));
    }
}
