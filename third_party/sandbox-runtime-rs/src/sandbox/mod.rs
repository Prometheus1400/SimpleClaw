//! Platform-specific sandbox implementations.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

use crate::config::SandboxRuntimeConfig;
use crate::error::SandboxError;
use crate::utils::Platform;

/// Detailed status of sandbox dependencies.
#[derive(Debug, Clone, Default)]
pub struct LinuxDependencyStatus {
    pub has_bwrap: bool,
    pub has_socat: bool,
    pub has_seccomp_bpf: bool,
    pub has_seccomp_apply: bool,
}

/// Result of checking sandbox dependencies.
#[derive(Debug, Clone, Default)]
pub struct SandboxDependencyCheck {
    /// Errors that prevent the sandbox from running.
    pub errors: Vec<String>,
    /// Warnings about degraded functionality.
    pub warnings: Vec<String>,
}

impl SandboxDependencyCheck {
    /// Returns true if there are no errors.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Convert to Result, returning error if there are any errors.
    pub fn into_result(self) -> Result<Self, SandboxError> {
        if self.errors.is_empty() {
            Ok(self)
        } else {
            Err(SandboxError::MissingDependency(self.errors.join(", ")))
        }
    }
}

/// Get detailed status of Linux sandbox dependencies.
#[cfg(target_os = "linux")]
pub fn get_linux_dependency_status(
    seccomp_config: Option<&crate::config::SeccompConfig>,
) -> LinuxDependencyStatus {
    LinuxDependencyStatus {
        has_bwrap: linux::check_bwrap(),
        has_socat: linux::check_socat(),
        has_seccomp_bpf: linux::get_bpf_path(seccomp_config).is_ok(),
        has_seccomp_apply: linux::get_apply_seccomp_path(seccomp_config).is_ok(),
    }
}

/// Check sandbox dependencies and return structured result.
#[cfg(target_os = "linux")]
pub fn check_linux_dependencies(
    seccomp_config: Option<&crate::config::SeccompConfig>,
) -> SandboxDependencyCheck {
    let mut result = SandboxDependencyCheck::default();

    if !linux::check_bwrap() {
        result.errors.push("bubblewrap (bwrap) not installed".to_string());
    }
    if !linux::check_socat() {
        result.errors.push("socat not installed".to_string());
    }

    let has_bpf = linux::get_bpf_path(seccomp_config).is_ok();
    let has_apply = linux::get_apply_seccomp_path(seccomp_config).is_ok();
    if !has_bpf || !has_apply {
        result.warnings.push(
            "seccomp not available - unix socket access not restricted".to_string(),
        );
    }

    result
}

/// Check if sandboxing dependencies are available for the current platform.
/// Returns a structured result with errors and warnings.
pub fn check_dependencies_detailed(
    platform: Platform,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    seccomp_config: Option<&crate::config::SeccompConfig>,
) -> SandboxDependencyCheck {
    match platform {
        Platform::MacOS => {
            // sandbox-exec is built into macOS
            SandboxDependencyCheck::default()
        }
        Platform::Linux => {
            #[cfg(target_os = "linux")]
            {
                check_linux_dependencies(seccomp_config)
            }
            #[cfg(not(target_os = "linux"))]
            {
                SandboxDependencyCheck {
                    errors: vec!["Linux sandbox code not compiled on this platform".to_string()],
                    warnings: vec![],
                }
            }
        }
    }
}

/// Check if sandboxing dependencies are available for the current platform.
/// Legacy function that returns Result for backward compatibility.
pub fn check_dependencies(platform: Platform) -> Result<(), SandboxError> {
    check_dependencies_detailed(platform, None).into_result().map(|_| ())
}

/// Wrap a command with platform-specific sandboxing.
pub async fn wrap_command(
    command: &str,
    config: &SandboxRuntimeConfig,
    platform: Platform,
    http_proxy_port: Option<u16>,
    socks_proxy_port: Option<u16>,
    #[cfg(target_os = "linux")] http_socket_path: Option<&str>,
    #[cfg(target_os = "linux")] socks_socket_path: Option<&str>,
    shell: Option<&str>,
    enable_log_monitor: bool,
) -> Result<WrapResult, SandboxError> {
    match platform {
        Platform::MacOS => {
            #[cfg(target_os = "macos")]
            {
                let (wrapped, log_tag) = macos::wrap_command(
                    command,
                    config,
                    http_proxy_port,
                    socks_proxy_port,
                    shell,
                    enable_log_monitor,
                )?;
                Ok(WrapResult {
                    command: wrapped,
                    log_tag,
                    warnings: vec![],
                })
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(SandboxError::UnsupportedPlatform(
                    "macOS sandbox code not compiled on this platform".to_string(),
                ))
            }
        }
        Platform::Linux => {
            #[cfg(target_os = "linux")]
            {
                let cwd = std::env::current_dir()?;
                let (wrapped, warnings) = linux::generate_bwrap_command(
                    command,
                    config,
                    &cwd,
                    http_socket_path,
                    socks_socket_path,
                    http_proxy_port.unwrap_or(3128),
                    socks_proxy_port.unwrap_or(1080),
                    shell,
                )?;
                Ok(WrapResult {
                    command: wrapped,
                    log_tag: None,
                    warnings,
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(SandboxError::UnsupportedPlatform(
                    "Linux sandbox code not compiled on this platform".to_string(),
                ))
            }
        }
    }
}

/// Result of wrapping a command with sandbox.
#[derive(Debug)]
pub struct WrapResult {
    /// The wrapped command string.
    pub command: String,
    /// Log tag for violation monitoring (macOS only).
    pub log_tag: Option<String>,
    /// Warnings generated during wrapping.
    pub warnings: Vec<String>,
}
