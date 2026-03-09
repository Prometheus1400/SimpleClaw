//! Violation monitoring via macOS log stream.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::error::SandboxError;
use crate::violation::SandboxViolationEvent;

/// Log monitor for sandbox violations.
#[allow(dead_code)]
pub struct LogMonitor {
    child: Option<Child>,
    log_tag: String,
    tx: mpsc::Sender<SandboxViolationEvent>,
}

impl LogMonitor {
    /// Start monitoring for violations with the given log tag.
    pub async fn start(
        log_tag: String,
        command: Option<String>,
    ) -> Result<(Self, mpsc::Receiver<SandboxViolationEvent>), SandboxError> {
        let (tx, rx) = mpsc::channel(100);

        // Start log stream process
        let child = Command::new("log")
            .args([
                "stream",
                "--predicate",
                &format!("subsystem == 'com.apple.sandbox' AND eventMessage CONTAINS '{}'", log_tag),
                "--style",
                "compact",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut monitor = Self {
            child: Some(child),
            log_tag: log_tag.clone(),
            tx: tx.clone(),
        };

        // Spawn a task to read the log stream
        let child = monitor.child.take();
        if let Some(mut child) = child {
            let log_tag_clone = log_tag.clone();
            let command_clone = command.clone();

            tokio::spawn(async move {
                if let Some(stdout) = child.stdout.take() {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();

                    while let Ok(Some(line)) = lines.next_line().await {
                        // Check if the line contains our log tag
                        if line.contains(&log_tag_clone) {
                            let event = SandboxViolationEvent::with_command(
                                line,
                                command_clone.clone(),
                                Some(log_tag_clone.clone()),
                            );

                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                }

                let _ = child.kill().await;
            });
        }

        Ok((monitor, rx))
    }

    /// Stop the log monitor.
    pub async fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill().await;
        }
    }
}

impl Drop for LogMonitor {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            // Try to kill the child process
            // Note: This is synchronous, but we're in Drop
            let _ = child.start_kill();
        }
    }
}

/// Parse a violation from a log line.
pub fn parse_violation(line: &str, log_tag: &str) -> Option<SandboxViolationEvent> {
    if line.contains(log_tag) {
        Some(SandboxViolationEvent::with_command(
            line.to_string(),
            None,
            Some(log_tag.to_string()),
        ))
    } else {
        None
    }
}

/// Decode the original command from the log tag.
pub fn decode_command_from_tag(tag: &str) -> Option<String> {
    use base64::Engine;

    // Format: CMD64_<base64>_END_<suffix>
    if let Some(start) = tag.find("CMD64_") {
        let rest = &tag[start + 6..];
        if let Some(end) = rest.find("_END_") {
            let encoded = &rest[..end];
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                return String::from_utf8(decoded).ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_command_from_tag() {
        use base64::Engine;
        let command = "echo hello";
        let encoded = base64::engine::general_purpose::STANDARD.encode(command);
        let tag = format!("CMD64_{}_END_12345678", encoded);

        let decoded = decode_command_from_tag(&tag);
        assert_eq!(decoded, Some("echo hello".to_string()));
    }
}
