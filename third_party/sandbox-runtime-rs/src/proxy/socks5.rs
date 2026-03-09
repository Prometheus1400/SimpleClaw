//! SOCKS5 proxy server (RFC 1928).

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::error::SandboxError;
use crate::proxy::filter::{DomainFilter, FilterDecision};

// SOCKS5 constants
const SOCKS_VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CONNECTION_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;

/// SOCKS5 proxy server.
pub struct Socks5Proxy {
    listener: Option<TcpListener>,
    port: u16,
    filter: Arc<DomainFilter>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Socks5Proxy {
    /// Create a new SOCKS5 proxy server.
    pub async fn new(filter: DomainFilter) -> Result<Self, SandboxError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tracing::debug!("SOCKS5 proxy listening on port {}", port);

        Ok(Self {
            listener: Some(listener),
            port,
            filter: Arc::new(filter),
            shutdown_tx: None,
        })
    }

    /// Get the port the proxy is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Start the proxy server.
    pub fn start(&mut self) -> Result<(), SandboxError> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| SandboxError::Proxy("Proxy already started".to_string()))?;

        let filter = self.filter.clone();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        self.shutdown_tx = Some(shutdown_tx);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, addr)) => {
                                let filter = filter.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_client(stream, addr, filter).await {
                                        tracing::debug!("SOCKS5 error from {}: {}", addr, e);
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("SOCKS5 accept error: {}", e);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::debug!("SOCKS5 proxy shutting down");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the proxy server.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Handle a SOCKS5 client connection.
async fn handle_client(
    mut stream: TcpStream,
    _addr: SocketAddr,
    filter: Arc<DomainFilter>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read version and authentication methods
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await?;

    if header[0] != SOCKS_VERSION {
        return Err("Invalid SOCKS version".into());
    }

    let nmethods = header[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    // We only support no authentication
    if !methods.contains(&AUTH_NONE) {
        stream.write_all(&[SOCKS_VERSION, 0xFF]).await?;
        return Err("No supported authentication method".into());
    }

    // Send auth method selection
    stream.write_all(&[SOCKS_VERSION, AUTH_NONE]).await?;

    // Read connection request
    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;

    if request[0] != SOCKS_VERSION {
        return Err("Invalid SOCKS version in request".into());
    }

    let cmd = request[1];
    // request[2] is reserved
    let atyp = request[3];

    if cmd != CMD_CONNECT {
        send_reply(&mut stream, REP_GENERAL_FAILURE, "0.0.0.0", 0).await?;
        return Err("Only CONNECT command is supported".into());
    }

    // Parse destination address
    let (host, port) = match atyp {
        ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let host = format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3]);
            (host, port)
        }
        ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            stream.read_exact(&mut domain).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let host = String::from_utf8_lossy(&domain).to_string();
            (host, port)
        }
        ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            // Format as IPv6 address
            let host = format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
                u16::from_be_bytes([addr[0], addr[1]]),
                u16::from_be_bytes([addr[2], addr[3]]),
                u16::from_be_bytes([addr[4], addr[5]]),
                u16::from_be_bytes([addr[6], addr[7]]),
                u16::from_be_bytes([addr[8], addr[9]]),
                u16::from_be_bytes([addr[10], addr[11]]),
                u16::from_be_bytes([addr[12], addr[13]]),
                u16::from_be_bytes([addr[14], addr[15]])
            );
            (host, port)
        }
        _ => {
            send_reply(&mut stream, REP_GENERAL_FAILURE, "0.0.0.0", 0).await?;
            return Err("Unsupported address type".into());
        }
    };

    tracing::debug!("SOCKS5 CONNECT {}:{}", host, port);

    // Check filter
    let decision = filter.check(&host, port);

    if matches!(decision, FilterDecision::Deny) {
        tracing::debug!("SOCKS5 denied connection to {}:{}", host, port);
        send_reply(&mut stream, REP_CONNECTION_NOT_ALLOWED, "0.0.0.0", 0).await?;
        return Ok(());
    }

    // Connect to target
    let target = match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("SOCKS5 failed to connect to {}:{}: {}", host, port, e);
            send_reply(&mut stream, REP_HOST_UNREACHABLE, "0.0.0.0", 0).await?;
            return Ok(());
        }
    };

    // Send success reply
    let local_addr = target.local_addr()?;
    let (bind_addr, bind_port) = match local_addr {
        SocketAddr::V4(addr) => (addr.ip().to_string(), addr.port()),
        SocketAddr::V6(addr) => (addr.ip().to_string(), addr.port()),
    };
    send_reply(&mut stream, REP_SUCCESS, &bind_addr, bind_port).await?;

    // Pipe data
    let (mut client_read, mut client_write) = stream.into_split();
    let (mut target_read, mut target_write) = target.into_split();

    let client_to_target = tokio::io::copy(&mut client_read, &mut target_write);
    let target_to_client = tokio::io::copy(&mut target_read, &mut client_write);

    tokio::select! {
        _ = client_to_target => {}
        _ = target_to_client => {}
    }

    Ok(())
}

/// Send a SOCKS5 reply.
async fn send_reply(
    stream: &mut TcpStream,
    rep: u8,
    addr: &str,
    port: u16,
) -> Result<(), std::io::Error> {
    let mut reply = vec![SOCKS_VERSION, rep, 0x00]; // VER, REP, RSV

    // Parse address
    if let Ok(ipv4) = addr.parse::<std::net::Ipv4Addr>() {
        reply.push(ATYP_IPV4);
        reply.extend_from_slice(&ipv4.octets());
    } else if let Ok(ipv6) = addr.parse::<std::net::Ipv6Addr>() {
        reply.push(ATYP_IPV6);
        reply.extend_from_slice(&ipv6.octets());
    } else {
        // Domain name
        reply.push(ATYP_DOMAIN);
        reply.push(addr.len() as u8);
        reply.extend_from_slice(addr.as_bytes());
    }

    reply.extend_from_slice(&port.to_be_bytes());

    stream.write_all(&reply).await
}
