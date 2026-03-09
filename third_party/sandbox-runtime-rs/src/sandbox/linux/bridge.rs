//! Socat Unix socket bridges for Linux network sandboxing.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::error::SandboxError;

/// A socat bridge between a Unix socket and a TCP port.
pub struct SocatBridge {
    child: Option<Child>,
    socket_path: PathBuf,
}

impl SocatBridge {
    /// Create a bridge from a Unix socket to a TCP port.
    /// The Unix socket will be created and listen for connections.
    /// Each connection will be forwarded to the TCP port.
    pub async fn unix_to_tcp(
        socket_path: PathBuf,
        tcp_host: &str,
        tcp_port: u16,
    ) -> Result<Self, SandboxError> {
        // Remove existing socket if present
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        let child = Command::new("socat")
            .args([
                &format!("UNIX-LISTEN:{},fork", socket_path.display()),
                &format!("TCP:{}:{}", tcp_host, tcp_port),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SandboxError::MissingDependency(
                        "socat not found. Please install socat.".to_string(),
                    )
                } else {
                    SandboxError::Io(e)
                }
            })?;

        // Wait a bit for the socket to be created
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        Ok(Self {
            child: Some(child),
            socket_path,
        })
    }

    /// Create a bridge from a TCP port to a Unix socket.
    /// This is used inside the sandbox to connect to the host proxies.
    pub fn tcp_to_unix_command(tcp_port: u16, socket_path: &str) -> String {
        format!(
            "socat TCP-LISTEN:{},fork,reuseaddr UNIX-CONNECT:{} </dev/null >/dev/null 2>&1",
            tcp_port, socket_path
        )
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Stop the bridge.
    pub async fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }

        // Clean up socket
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

impl Drop for SocatBridge {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }

        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// Check if socat is available.
pub fn check_socat() -> bool {
    std::process::Command::new("socat")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate a unique socket path in /tmp.
pub fn generate_socket_path(prefix: &str) -> PathBuf {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let suffix: u32 = rng.gen();
    PathBuf::from(format!("/tmp/{}-{}-{:08x}.sock", prefix, std::process::id(), suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_socket_path() {
        let path1 = generate_socket_path("srt-http");
        let path2 = generate_socket_path("srt-http");

        assert!(path1.to_string_lossy().starts_with("/tmp/srt-http-"));
        assert!(path1.to_string_lossy().ends_with(".sock"));
        // Paths should be different due to random suffix
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_tcp_to_unix_command() {
        let cmd = SocatBridge::tcp_to_unix_command(3128, "/tmp/http.sock");
        assert_eq!(
            cmd,
            "socat TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:/tmp/http.sock </dev/null >/dev/null 2>&1"
        );
    }
}
