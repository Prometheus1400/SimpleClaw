//! Sandbox manager - main orchestration module.

pub mod filesystem;
pub mod network;
pub mod state;

use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::SandboxRuntimeConfig;
use crate::error::SandboxError;
use crate::utils::{current_platform, check_ripgrep, Platform};
use crate::violation::SandboxViolationStore;

use self::state::ManagerState;

pub use filesystem::{FsReadRestrictionConfig, FsWriteRestrictionConfig};

/// The sandbox manager - main entry point for sandbox operations.
pub struct SandboxManager {
    state: Arc<RwLock<ManagerState>>,
}

impl Default for SandboxManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxManager {
    /// Create a new sandbox manager.
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ManagerState::new())),
        }
    }

    /// Check if the current platform is supported.
    pub fn is_supported_platform() -> bool {
        current_platform().is_some()
    }

    /// Check if all required dependencies are available.
    pub fn check_dependencies(&self, config: Option<&SandboxRuntimeConfig>) -> Result<(), SandboxError> {
        let platform = current_platform()
            .ok_or_else(|| SandboxError::UnsupportedPlatform("Unsupported platform".to_string()))?;

        // Check platform-specific dependencies
        crate::sandbox::check_dependencies(platform)?;

        // Check ripgrep (optional on macOS, recommended on Linux)
        if platform == Platform::Linux {
            let rg_config = config.and_then(|c| c.ripgrep.as_ref());
            if !check_ripgrep(rg_config) {
                tracing::warn!("ripgrep not found - dangerous file detection will be limited");
            }
        }

        Ok(())
    }

    /// Initialize the sandbox manager with the given configuration.
    pub async fn initialize(&self, config: SandboxRuntimeConfig) -> Result<(), SandboxError> {
        // Validate configuration
        config.validate()?;

        // Check dependencies
        self.check_dependencies(Some(&config))?;

        let platform = current_platform()
            .ok_or_else(|| SandboxError::UnsupportedPlatform("Unsupported platform".to_string()))?;

        // Initialize proxies
        let (http_proxy, socks_proxy) =
            network::initialize_proxies(&config.network).await?;

        let http_port = http_proxy.port();
        let socks_port = socks_proxy.port();

        // Update state
        let mut state = self.state.write();
        state.http_proxy = Some(http_proxy);
        state.socks_proxy = Some(socks_proxy);
        state.http_proxy_port = Some(http_port);
        state.socks_proxy_port = Some(socks_port);

        // Initialize platform-specific infrastructure
        #[cfg(target_os = "linux")]
        {
            use crate::sandbox::linux::{generate_socket_path, SocatBridge};

            // Create Unix socket bridges for proxies
            let http_socket_path = generate_socket_path("srt-http");
            let socks_socket_path = generate_socket_path("srt-socks");

            let http_bridge =
                SocatBridge::unix_to_tcp(http_socket_path.clone(), "localhost", http_port).await?;
            let socks_bridge =
                SocatBridge::unix_to_tcp(socks_socket_path.clone(), "localhost", socks_port)
                    .await?;

            state.http_socket_path = Some(http_socket_path.display().to_string());
            state.socks_socket_path = Some(socks_socket_path.display().to_string());
            state.bridges.push(http_bridge);
            state.bridges.push(socks_bridge);
        }

        state.config = Some(config);
        state.initialized = true;
        state.network_ready = true;

        tracing::info!(
            "Sandbox manager initialized for {} (HTTP proxy: {}, SOCKS proxy: {})",
            platform.name(),
            http_port,
            socks_port
        );

        Ok(())
    }

    /// Check if the manager is initialized.
    pub fn is_initialized(&self) -> bool {
        self.state.read().initialized
    }

    /// Get the current configuration.
    pub fn get_config(&self) -> Option<SandboxRuntimeConfig> {
        self.state.read().config.clone()
    }

    /// Update the configuration.
    pub fn update_config(&self, config: SandboxRuntimeConfig) -> Result<(), SandboxError> {
        config.validate()?;
        self.state.write().config = Some(config);
        Ok(())
    }

    /// Get the HTTP proxy port.
    pub fn get_proxy_port(&self) -> Option<u16> {
        self.state.read().http_proxy_port
    }

    /// Get the SOCKS proxy port.
    pub fn get_socks_proxy_port(&self) -> Option<u16> {
        self.state.read().socks_proxy_port
    }

    /// Get the HTTP socket path (Linux only).
    #[cfg(target_os = "linux")]
    pub fn get_http_socket_path(&self) -> Option<String> {
        self.state.read().http_socket_path.clone()
    }

    /// Get the SOCKS socket path (Linux only).
    #[cfg(target_os = "linux")]
    pub fn get_socks_socket_path(&self) -> Option<String> {
        self.state.read().socks_socket_path.clone()
    }

    /// Check if network is ready.
    pub fn is_network_ready(&self) -> bool {
        self.state.read().network_ready
    }

    /// Wait for network initialization.
    pub async fn wait_for_network_initialization(&self) -> bool {
        // Already ready in this implementation since we initialize synchronously
        self.is_network_ready()
    }

    /// Get filesystem read restriction config.
    pub fn get_fs_read_config(&self) -> FsReadRestrictionConfig {
        let state = self.state.read();
        if let Some(ref config) = state.config {
            filesystem::process_fs_config(&config.filesystem).0
        } else {
            FsReadRestrictionConfig::default()
        }
    }

    /// Get filesystem write restriction config.
    pub fn get_fs_write_config(&self) -> FsWriteRestrictionConfig {
        let state = self.state.read();
        if let Some(ref config) = state.config {
            filesystem::process_fs_config(&config.filesystem).1
        } else {
            FsWriteRestrictionConfig::default()
        }
    }

    /// Get glob pattern warnings for Linux.
    pub fn get_linux_glob_pattern_warnings(&self) -> Vec<String> {
        #[cfg(target_os = "linux")]
        {
            let state = self.state.read();
            if let Some(ref config) = state.config {
                let mut warnings = Vec::new();
                for path in &config.filesystem.allow_write {
                    if crate::utils::contains_glob_chars(path) {
                        warnings.push(format!(
                            "Glob pattern '{}' is not supported on Linux",
                            path
                        ));
                    }
                }
                for path in &config.filesystem.deny_write {
                    if crate::utils::contains_glob_chars(path) {
                        warnings.push(format!(
                            "Glob pattern '{}' is not supported on Linux",
                            path
                        ));
                    }
                }
                return warnings;
            }
        }
        Vec::new()
    }

    /// Get the violation store.
    pub fn get_violation_store(&self) -> Arc<SandboxViolationStore> {
        self.state.read().violation_store.clone()
    }

    /// Wrap a command with sandbox restrictions.
    pub async fn wrap_with_sandbox(
        &self,
        command: &str,
        shell: Option<&str>,
        custom_config: Option<SandboxRuntimeConfig>,
    ) -> Result<String, SandboxError> {
        // Extract needed values from state while holding the lock
        let (config, http_port, socks_port) = {
            let state = self.state.read();

            if !state.initialized {
                return Err(SandboxError::ExecutionFailed(
                    "Sandbox manager not initialized".to_string(),
                ));
            }

            let config = custom_config
                .or_else(|| state.config.clone())
                .ok_or_else(|| SandboxError::ExecutionFailed("No configuration available".to_string()))?;

            (config, state.http_proxy_port, state.socks_proxy_port)
        };

        let _platform = current_platform()
            .ok_or_else(|| SandboxError::UnsupportedPlatform("Unsupported platform".to_string()))?;

        // Call platform-specific wrapper
        #[cfg(target_os = "macos")]
        {
            let (wrapped, _log_tag) = crate::sandbox::macos::wrap_command(
                command,
                &config,
                http_port,
                socks_port,
                shell,
                true, // enable log monitor
            )?;
            Ok(wrapped)
        }

        #[cfg(target_os = "linux")]
        {
            let (http_socket, socks_socket) = {
                let state = self.state.read();
                (state.http_socket_path.clone(), state.socks_socket_path.clone())
            };

            let cwd = std::env::current_dir()?;
            let (wrapped, warnings) = crate::sandbox::linux::generate_bwrap_command(
                command,
                &config,
                &cwd,
                http_socket.as_deref(),
                socks_socket.as_deref(),
                http_port.unwrap_or(3128),
                socks_port.unwrap_or(1080),
                shell,
            )?;

            for warning in warnings {
                tracing::warn!("{}", warning);
            }

            Ok(wrapped)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Err(SandboxError::UnsupportedPlatform(
                "Platform not supported".to_string(),
            ))
        }
    }

    /// Annotate stderr with sandbox failure information.
    pub fn annotate_stderr_with_sandbox_failures(&self, command: &str, stderr: &str) -> String {
        let store = self.get_violation_store();
        let violations = store.get_violations_for_command(command);

        if violations.is_empty() {
            return stderr.to_string();
        }

        let mut annotated = stderr.to_string();
        annotated.push_str("\n\n--- Sandbox Violations ---\n");
        for violation in violations {
            annotated.push_str(&format!("  {}\n", violation.line));
        }

        annotated
    }

    /// Reset the sandbox manager, cleaning up all resources.
    pub async fn reset(&self) {
        // Clean up temp files on macOS
        #[cfg(target_os = "macos")]
        {
            crate::sandbox::macos::cleanup_temp_profiles();
        }

        let mut state = self.state.write();
        // We need to release the lock before calling async reset
        // So we'll just do the cleanup inline

        // Stop proxies
        if let Some(ref mut proxy) = state.http_proxy {
            proxy.stop();
        }
        if let Some(ref mut proxy) = state.socks_proxy {
            proxy.stop();
        }

        // Stop bridges (Linux)
        #[cfg(target_os = "linux")]
        {
            // Note: We can't call async stop here, so we rely on Drop
            state.bridges.clear();
            state.http_socket_path = None;
            state.socks_socket_path = None;
        }

        // Clear state
        state.http_proxy = None;
        state.socks_proxy = None;
        state.http_proxy_port = None;
        state.socks_proxy_port = None;
        state.config = None;
        state.initialized = false;
        state.network_ready = false;

        tracing::info!("Sandbox manager reset");
    }
}

impl Drop for SandboxManager {
    fn drop(&mut self) {
        // Cleanup is handled by reset() or individual component Drop implementations
    }
}
