//! Bubblewrap command generation for Linux sandbox.

use std::path::Path;

use crate::config::SandboxRuntimeConfig;
use crate::error::SandboxError;
use crate::sandbox::linux::bridge::SocatBridge;
use crate::sandbox::linux::filesystem::{generate_bind_mounts, BindMount};
use crate::sandbox::linux::seccomp::{get_apply_seccomp_path, get_bpf_path};
use crate::utils::quote;

/// Check if bubblewrap is available.
pub fn check_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate the bubblewrap command for sandboxed execution.
pub fn generate_bwrap_command(
    command: &str,
    config: &SandboxRuntimeConfig,
    cwd: &Path,
    http_socket_path: Option<&str>,
    socks_socket_path: Option<&str>,
    http_proxy_port: u16,
    socks_proxy_port: u16,
    shell: Option<&str>,
) -> Result<(String, Vec<String>), SandboxError> {
    let shell = shell.unwrap_or("/bin/bash");

    // Generate filesystem mounts
    let (mounts, warnings) = generate_bind_mounts(
        &config.filesystem,
        cwd,
        config.ripgrep.as_ref(),
        config.mandatory_deny_search_depth,
    )?;

    // Build bwrap arguments
    let mut bwrap_args = vec![
        "bwrap".to_string(),
        "--unshare-net".to_string(), // Network isolation
    ];

    // Start with read-only root filesystem. This must be mounted before
    // synthetic mounts like /dev, /proc, and tmpfs paths so those mounts
    // remain visible and usable in the final namespace.
    bwrap_args.push("--ro-bind".to_string());
    bwrap_args.push("/".to_string());
    bwrap_args.push("/".to_string());

    // Base synthetic filesystems/mounts.
    bwrap_args.push("--dev".to_string());
    bwrap_args.push("/dev".to_string());
    bwrap_args.push("--proc".to_string());
    bwrap_args.push("/proc".to_string());
    bwrap_args.push("--tmpfs".to_string());
    bwrap_args.push("/tmp".to_string());
    bwrap_args.push("--tmpfs".to_string());
    bwrap_args.push("/run".to_string());

    // Add writable mounts
    for mount in &mounts {
        if !mount.readonly {
            bwrap_args.extend(mount.to_bwrap_args());
        }
    }

    // Add read-only (deny) mounts to override writable ones
    for mount in &mounts {
        if mount.readonly {
            bwrap_args.extend(mount.to_bwrap_args());
        }
    }

    // Set working directory
    bwrap_args.push("--chdir".to_string());
    bwrap_args.push(cwd.display().to_string());

    // Build the inner command with socat bridges and seccomp
    let inner_command = build_inner_command(
        command,
        config,
        http_socket_path,
        socks_socket_path,
        http_proxy_port,
        socks_proxy_port,
        shell,
    )?;

    // Add the command
    bwrap_args.push("--".to_string());
    bwrap_args.push(shell.to_string());
    bwrap_args.push("-c".to_string());
    bwrap_args.push(inner_command);

    // Join into a single command string
    let wrapped = bwrap_args
        .iter()
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(" ");

    Ok((wrapped, warnings))
}

/// Build the inner command to run inside bubblewrap.
/// This sets up socat bridges and applies seccomp before running the user command.
fn build_inner_command(
    command: &str,
    config: &SandboxRuntimeConfig,
    http_socket_path: Option<&str>,
    socks_socket_path: Option<&str>,
    http_proxy_port: u16,
    socks_proxy_port: u16,
    shell: &str,
) -> Result<String, SandboxError> {
    let mut parts = Vec::new();
    let mut bridge_cmds = Vec::new();

    // Set up socat bridges for proxy access
    if let Some(http_sock) = http_socket_path {
        let bridge_cmd = SocatBridge::tcp_to_unix_command(http_proxy_port, http_sock);
        bridge_cmds.push(bridge_cmd);
    }

    if let Some(socks_sock) = socks_socket_path {
        let bridge_cmd = SocatBridge::tcp_to_unix_command(socks_proxy_port, socks_sock);
        bridge_cmds.push(bridge_cmd);
    }

    // Start proxy bridges in the background, then give them a moment to bind.
    if !bridge_cmds.is_empty() {
        parts.push(format!("{} & sleep 0.1", bridge_cmds.join(" & ")));
    }

    // Apply seccomp filter and execute command
    if !config.network.allow_all_unix_sockets.unwrap_or(false) {
        // Try to use seccomp to block Unix socket creation
        if let (Ok(bpf_path), Ok(apply_path)) = (
            get_bpf_path(config.seccomp.as_ref()),
            get_apply_seccomp_path(config.seccomp.as_ref()),
        ) {
            // Export proxy environment variables before applying seccomp
            let env_vars = generate_proxy_env_string(http_proxy_port, socks_proxy_port);
            parts.push(env_vars);

            // Use apply-seccomp to apply the filter and exec the command
            parts.push(format!(
                "{} {} {} -c {}",
                apply_path.display(),
                bpf_path.display(),
                shell,
                quote(command)
            ));
        } else {
            // Seccomp not available, just run the command with warning
            tracing::warn!(
                "Seccomp not available - Unix socket creation will not be blocked"
            );
            let env_vars = generate_proxy_env_string(http_proxy_port, socks_proxy_port);
            parts.push(format!("{} ; {} -c {}", env_vars, shell, quote(command)));
        }
    } else {
        // Unix sockets allowed, just run the command
        let env_vars = generate_proxy_env_string(http_proxy_port, socks_proxy_port);
        parts.push(format!("{} ; {} -c {}", env_vars, shell, quote(command)));
    }

    Ok(parts.join(" ; "))
}

/// Generate proxy environment variable exports.
fn generate_proxy_env_string(http_port: u16, socks_port: u16) -> String {
    format!(
        "export http_proxy='http://localhost:{}' https_proxy='http://localhost:{}' \
         HTTP_PROXY='http://localhost:{}' HTTPS_PROXY='http://localhost:{}' \
         ALL_PROXY='socks5://localhost:{}' all_proxy='socks5://localhost:{}'",
        http_port, http_port, http_port, http_port, socks_port, socks_port
    )
}

/// Generate proxy environment variables.
pub fn generate_proxy_env(http_port: u16, socks_port: u16) -> Vec<(String, String)> {
    let http_proxy = format!("http://localhost:{}", http_port);
    let socks_proxy = format!("socks5://localhost:{}", socks_port);

    vec![
        ("http_proxy".to_string(), http_proxy.clone()),
        ("HTTP_PROXY".to_string(), http_proxy.clone()),
        ("https_proxy".to_string(), http_proxy.clone()),
        ("HTTPS_PROXY".to_string(), http_proxy),
        ("ALL_PROXY".to_string(), socks_proxy.clone()),
        ("all_proxy".to_string(), socks_proxy),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_proxy_env_string() {
        let env = generate_proxy_env_string(3128, 1080);
        assert!(env.contains("http_proxy='http://localhost:3128'"));
        assert!(env.contains("ALL_PROXY='socks5://localhost:1080'"));
        assert!(!env.ends_with(';'));
    }

    #[test]
    fn test_build_inner_command_does_not_emit_double_separators() {
        let mut config = SandboxRuntimeConfig::default();
        config.network.allow_all_unix_sockets = Some(true);

        let inner = build_inner_command(
            "echo ok",
            &config,
            Some("/tmp/http.sock"),
            Some("/tmp/socks.sock"),
            3128,
            1080,
            "/bin/bash",
        )
        .expect("build_inner_command should succeed");

        assert!(!inner.contains("& ;"), "unexpected '& ;' in: {inner}");
        assert!(!inner.contains("; ;"), "unexpected '; ;' in: {inner}");
    }

    #[test]
    fn test_build_inner_command_splits_export_and_shell_exec() {
        let mut config = SandboxRuntimeConfig::default();
        config.network.allow_all_unix_sockets = Some(true);

        let inner = build_inner_command("ls", &config, None, None, 3128, 1080, "/bin/bash")
            .expect("build_inner_command should succeed");

        assert!(
            inner.contains("all_proxy='socks5://localhost:1080' ; /bin/bash -c 'ls'"),
            "inner command must separate export from shell execution: {inner}"
        );
    }

    #[test]
    fn test_generate_bwrap_command_uses_provided_cwd_for_chdir() {
        let mut config = SandboxRuntimeConfig::default();
        config.network.allow_all_unix_sockets = Some(true);
        let cwd = Path::new("/tmp/simpleclaw-workspace");

        let (wrapped, _warnings) = generate_bwrap_command(
            "pwd",
            &config,
            cwd,
            None,
            None,
            3128,
            1080,
            Some("/bin/bash"),
        )
        .expect("generate_bwrap_command should succeed");

        assert!(
            wrapped.contains("--chdir /tmp/simpleclaw-workspace"),
            "wrapped command should use provided cwd: {wrapped}"
        );
    }

    #[test]
    fn test_check_bwrap() {
        // This test will pass/fail based on system configuration
        let available = check_bwrap();
        println!("Bubblewrap available: {}", available);
    }
}
