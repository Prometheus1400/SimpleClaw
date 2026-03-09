//! Network initialization and management.

use crate::config::NetworkConfig;
use crate::error::SandboxError;
use crate::proxy::{DomainFilter, HttpProxy, Socks5Proxy};

/// Initialize network proxies.
pub async fn initialize_proxies(
    config: &NetworkConfig,
) -> Result<(HttpProxy, Socks5Proxy), SandboxError> {
    // Create domain filter from config
    let filter = DomainFilter::from_config(config);

    // Get MITM socket path if configured
    let mitm_socket_path = config.mitm_proxy.as_ref().map(|m| m.socket_path.clone());

    // Create HTTP proxy
    let mut http_proxy = HttpProxy::new(filter.clone(), mitm_socket_path).await?;
    http_proxy.start()?;

    // Create SOCKS5 proxy
    let mut socks_proxy = Socks5Proxy::new(filter).await?;
    socks_proxy.start()?;

    tracing::debug!(
        "Proxies started - HTTP: {}, SOCKS5: {}",
        http_proxy.port(),
        socks_proxy.port()
    );

    Ok((http_proxy, socks_proxy))
}

/// Generate proxy environment variables for sandboxed commands.
#[allow(dead_code)]
pub fn generate_proxy_env_vars(
    http_port: u16,
    socks_port: u16,
    http_socket_path: Option<&str>,
    _socks_socket_path: Option<&str>,
) -> Vec<(String, String)> {
    let http_proxy = if let Some(_socket) = http_socket_path {
        // On Linux, use localhost inside the sandbox (socat bridges to socket)
        format!("http://localhost:{}", http_port)
    } else {
        format!("http://localhost:{}", http_port)
    };

    let socks_proxy = format!("socks5://localhost:{}", socks_port);

    let mut env = vec![
        ("http_proxy".to_string(), http_proxy.clone()),
        ("HTTP_PROXY".to_string(), http_proxy.clone()),
        ("https_proxy".to_string(), http_proxy.clone()),
        ("HTTPS_PROXY".to_string(), http_proxy),
        ("ALL_PROXY".to_string(), socks_proxy.clone()),
        ("all_proxy".to_string(), socks_proxy.clone()),
    ];

    // Git SSH command for SOCKS proxy
    env.push((
        "GIT_SSH_COMMAND".to_string(),
        format!(
            "ssh -o ProxyCommand='nc -X 5 -x localhost:{} %h %p'",
            socks_port
        ),
    ));

    env
}
