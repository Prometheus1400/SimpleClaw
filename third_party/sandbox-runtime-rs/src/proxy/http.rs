//! HTTP/HTTPS proxy server with CONNECT tunneling support.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;

use crate::error::SandboxError;
use crate::proxy::filter::{DomainFilter, FilterDecision};

/// HTTP proxy server.
pub struct HttpProxy {
    listener: Option<TcpListener>,
    port: u16,
    filter: Arc<DomainFilter>,
    mitm_socket_path: Option<String>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl HttpProxy {
    /// Create a new HTTP proxy server.
    pub async fn new(
        filter: DomainFilter,
        mitm_socket_path: Option<String>,
    ) -> Result<Self, SandboxError> {
        // Bind to localhost on any available port
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        tracing::debug!("HTTP proxy listening on port {}", port);

        Ok(Self {
            listener: Some(listener),
            port,
            filter: Arc::new(filter),
            mitm_socket_path,
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
        let mitm_socket_path = self.mitm_socket_path.clone();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        self.shutdown_tx = Some(shutdown_tx);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, addr)) => {
                                let filter = filter.clone();
                                let mitm_socket = mitm_socket_path.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, addr, filter, mitm_socket).await {
                                        tracing::debug!("Connection error from {}: {}", addr, e);
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Accept error: {}", e);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::debug!("HTTP proxy shutting down");
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

/// Handle a single proxy connection.
async fn handle_connection(
    stream: TcpStream,
    _addr: SocketAddr,
    filter: Arc<DomainFilter>,
    mitm_socket_path: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let io = TokioIo::new(stream);

    let filter_clone = filter.clone();
    let mitm_socket_clone = mitm_socket_path.clone();

    http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            io,
            service_fn(move |req| {
                let filter = filter_clone.clone();
                let mitm_socket = mitm_socket_clone.clone();
                async move { handle_request(req, filter, mitm_socket).await }
            }),
        )
        .with_upgrades()
        .await?;

    Ok(())
}

/// Handle a single HTTP request.
async fn handle_request(
    req: Request<hyper::body::Incoming>,
    filter: Arc<DomainFilter>,
    mitm_socket_path: Option<String>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    if req.method() == Method::CONNECT {
        handle_connect(req, filter, mitm_socket_path).await
    } else {
        handle_http(req, filter, mitm_socket_path).await
    }
}

/// Handle CONNECT requests (HTTPS tunneling).
async fn handle_connect(
    req: Request<hyper::body::Incoming>,
    filter: Arc<DomainFilter>,
    mitm_socket_path: Option<String>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let host = req.uri().host().unwrap_or_default().to_string();
    let port = req.uri().port_u16().unwrap_or(443);

    tracing::debug!("CONNECT {}:{}", host, port);

    // Check filter
    let decision = filter.check(&host, port);

    match decision {
        FilterDecision::Deny => {
            tracing::debug!("Denied CONNECT to {}:{}", host, port);
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(empty_body())
                .unwrap());
        }
        FilterDecision::Mitm => {
            // Route through MITM proxy via Unix socket
            if let Some(socket_path) = mitm_socket_path {
                return handle_connect_mitm(req, &socket_path, &host, port).await;
            }
        }
        FilterDecision::Allow => {}
    }

    // Direct tunnel
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = tunnel(upgraded, &host, port).await {
                    tracing::debug!("Tunnel error: {}", e);
                }
            }
            Err(e) => {
                tracing::debug!("Upgrade error: {}", e);
            }
        }
    });

    Ok(Response::new(empty_body()))
}

/// Handle CONNECT through MITM proxy.
async fn handle_connect_mitm(
    req: Request<hyper::body::Incoming>,
    socket_path: &str,
    host: &str,
    port: u16,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let socket_path = socket_path.to_string();
    let host = host.to_string();

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = tunnel_via_mitm(upgraded, &socket_path, &host, port).await {
                    tracing::debug!("MITM tunnel error: {}", e);
                }
            }
            Err(e) => {
                tracing::debug!("Upgrade error: {}", e);
            }
        }
    });

    Ok(Response::new(empty_body()))
}

/// Tunnel data between upgraded connection and target.
async fn tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target = TcpStream::connect(format!("{}:{}", host, port)).await?;

    let mut upgraded = TokioIo::new(upgraded);
    let (mut target_read, mut target_write) = target.into_split();
    let (mut client_read, mut client_write) = tokio::io::split(&mut upgraded);

    let client_to_server = tokio::io::copy(&mut client_read, &mut target_write);
    let server_to_client = tokio::io::copy(&mut target_read, &mut client_write);

    tokio::try_join!(client_to_server, server_to_client)?;

    Ok(())
}

/// Tunnel through MITM proxy via Unix socket.
async fn tunnel_via_mitm(
    upgraded: hyper::upgrade::Upgraded,
    socket_path: &str,
    host: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut mitm_stream = UnixStream::connect(socket_path).await?;

    // Send CONNECT request to MITM proxy
    let connect_req = format!("CONNECT {}:{} HTTP/1.1\r\nHost: {}:{}\r\n\r\n", host, port, host, port);
    mitm_stream.write_all(connect_req.as_bytes()).await?;

    // Read response (should be 200 Connection Established)
    let mut response_buf = [0u8; 1024];
    let n = mitm_stream.read(&mut response_buf).await?;
    let response = String::from_utf8_lossy(&response_buf[..n]);

    if !response.contains("200") {
        return Err(format!("MITM proxy returned: {}", response).into());
    }

    // Pipe the upgraded connection to the MITM socket
    let mut upgraded = TokioIo::new(upgraded);
    let (mut mitm_read, mut mitm_write) = mitm_stream.into_split();
    let (mut client_read, mut client_write) = tokio::io::split(&mut upgraded);

    let client_to_server = tokio::io::copy(&mut client_read, &mut mitm_write);
    let server_to_client = tokio::io::copy(&mut mitm_read, &mut client_write);

    tokio::try_join!(client_to_server, server_to_client)?;

    Ok(())
}

/// Handle regular HTTP requests.
async fn handle_http(
    req: Request<hyper::body::Incoming>,
    filter: Arc<DomainFilter>,
    mitm_socket_path: Option<String>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let host = req
        .uri()
        .host()
        .or_else(|| {
            req.headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|h| h.split(':').next().unwrap_or(h))
        })
        .unwrap_or_default()
        .to_string();

    let port = req.uri().port_u16().unwrap_or(80);

    tracing::debug!("HTTP {} {}:{}", req.method(), host, port);

    // Check filter
    let decision = filter.check(&host, port);

    if matches!(decision, FilterDecision::Deny) {
        tracing::debug!("Denied HTTP to {}:{}", host, port);
        return Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(full_body("Access denied by sandbox policy"))
            .unwrap());
    }

    // Route through MITM if needed
    if matches!(decision, FilterDecision::Mitm) {
        if let Some(socket_path) = mitm_socket_path {
            return forward_http_via_mitm(req, &socket_path).await;
        }
    }

    // Forward the request directly
    forward_http(req).await
}

/// Forward HTTP request directly to target.
async fn forward_http(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let host = req
        .uri()
        .host()
        .unwrap_or_default()
        .to_string();
    let port = req.uri().port_u16().unwrap_or(80);

    // Connect to target
    let stream = match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("Failed to connect to {}:{}: {}", host, port, e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full_body("Failed to connect to target"))
                .unwrap());
        }
    };

    let io = TokioIo::new(stream);

    let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("Handshake error: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full_body("Handshake failed"))
                .unwrap());
        }
    };

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("Connection error: {}", e);
        }
    });

    match sender.send_request(req).await {
        Ok(resp) => Ok(resp.map(|b| b.boxed())),
        Err(e) => {
            tracing::debug!("Request error: {}", e);
            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full_body("Request failed"))
                .unwrap())
        }
    }
}

/// Forward HTTP request via MITM Unix socket.
async fn forward_http_via_mitm(
    _req: Request<hyper::body::Incoming>,
    _socket_path: &str,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    // TODO: Implement HTTP forwarding via Unix socket
    Ok(Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(full_body("MITM HTTP forwarding not implemented"))
        .unwrap())
}

fn empty_body() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(s: &str) -> BoxBody<Bytes, hyper::Error> {
    Full::new(Bytes::from(s.to_string()))
        .map_err(|never| match never {})
        .boxed()
}
