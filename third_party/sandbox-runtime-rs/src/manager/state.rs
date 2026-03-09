//! Global state management for the sandbox manager.

use std::sync::Arc;


use crate::config::SandboxRuntimeConfig;
use crate::proxy::{HttpProxy, Socks5Proxy};
use crate::violation::SandboxViolationStore;

/// Internal state for the sandbox manager.
pub struct ManagerState {
    /// The current configuration.
    pub config: Option<SandboxRuntimeConfig>,

    /// HTTP proxy server.
    pub http_proxy: Option<HttpProxy>,

    /// SOCKS5 proxy server.
    pub socks_proxy: Option<Socks5Proxy>,

    /// HTTP proxy port.
    pub http_proxy_port: Option<u16>,

    /// SOCKS5 proxy port.
    pub socks_proxy_port: Option<u16>,

    /// Unix socket path for HTTP proxy (Linux only).
    #[cfg(target_os = "linux")]
    pub http_socket_path: Option<String>,

    /// Unix socket path for SOCKS5 proxy (Linux only).
    #[cfg(target_os = "linux")]
    pub socks_socket_path: Option<String>,

    /// Socat bridge processes (Linux only).
    #[cfg(target_os = "linux")]
    pub bridges: Vec<crate::sandbox::linux::SocatBridge>,

    /// Whether the manager has been initialized.
    pub initialized: bool,

    /// Whether network is ready.
    pub network_ready: bool,

    /// Violation store.
    pub violation_store: Arc<SandboxViolationStore>,
}

impl Default for ManagerState {
    fn default() -> Self {
        Self {
            config: None,
            http_proxy: None,
            socks_proxy: None,
            http_proxy_port: None,
            socks_proxy_port: None,
            #[cfg(target_os = "linux")]
            http_socket_path: None,
            #[cfg(target_os = "linux")]
            socks_socket_path: None,
            #[cfg(target_os = "linux")]
            bridges: Vec::new(),
            initialized: false,
            network_ready: false,
            violation_store: Arc::new(SandboxViolationStore::new()),
        }
    }
}

impl ManagerState {
    /// Create a new manager state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the state, cleaning up resources.
    pub async fn reset(&mut self) {
        // Stop proxies
        if let Some(ref mut proxy) = self.http_proxy {
            proxy.stop();
        }
        if let Some(ref mut proxy) = self.socks_proxy {
            proxy.stop();
        }

        // Stop bridges (Linux)
        #[cfg(target_os = "linux")]
        {
            for bridge in &mut self.bridges {
                bridge.stop().await;
            }
            self.bridges.clear();
            self.http_socket_path = None;
            self.socks_socket_path = None;
        }

        // Clear state
        self.http_proxy = None;
        self.socks_proxy = None;
        self.http_proxy_port = None;
        self.socks_proxy_port = None;
        self.config = None;
        self.initialized = false;
        self.network_ready = false;
    }
}
