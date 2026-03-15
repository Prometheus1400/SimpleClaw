//! Tool-agnostic sandbox runtime interfaces and implementations.

mod host;
mod wasm;

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use sandbox_runtime::{SandboxError, SandboxedChild, SandboxedCommand};
use tracing::warn;

use crate::error::FrameworkError;

pub(crate) use host::DefaultHostSandbox;
pub(crate) use wasm::{
    DefaultWasmSandbox, normalize_workspace_root, persona_guest_mount_path,
    workspace_guest_mount_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasmPreopenedDir {
    pub host_path: PathBuf,
    pub guest_path: String,
}

/// Request for executing a WASM guest tool.
pub(crate) struct RunWasmRequest {
    pub workspace_root: PathBuf,
    pub persona_root: PathBuf,
    pub preopened_dirs: Vec<WasmPreopenedDir>,
    pub artifact_name: &'static str,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

/// Result from a completed WASM guest run.
pub(crate) struct WasmRunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Request for running a host command synchronously inside the sandbox.
pub(crate) struct RunHostCommandRequest {
    pub command: String,
    pub workspace_root: PathBuf,
    pub policy: SandboxPolicy,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: u64,
}

/// Request for preparing a host command for background execution.
pub(crate) struct SpawnHostCommandRequest {
    pub command: String,
    pub workspace_root: PathBuf,
    pub policy: SandboxPolicy,
}

/// Tool-agnostic sandbox policy inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SandboxPolicy {
    pub network_enabled: bool,
    pub extra_writable_paths: Vec<String>,
}

/// Result from a completed host command run.
pub(crate) struct HostRunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Whether the sandbox violation monitor detected seatbelt denials.
    pub sandbox_violated: bool,
}

/// Prepared host command ready for spawning.
pub(crate) struct PreparedHostCommand {
    command: SandboxedCommand,
}

/// Spawned sandboxed host process.
pub(crate) struct SpawnedHostCommand {
    child: SandboxedChild,
}

impl PreparedHostCommand {
    /// Spawns the prepared command with caller-provided stdio handles.
    pub(crate) async fn spawn(
        mut self,
        env: &BTreeMap<String, String>,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
    ) -> Result<SpawnedHostCommand, FrameworkError> {
        let child = self
            .command
            .envs(env)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .await
            .map_err(|e| {
                FrameworkError::Tool(format!("exec failed to start sandbox runtime: {e}"))
            })?;
        Ok(SpawnedHostCommand { child })
    }
}

impl SpawnedHostCommand {
    /// Returns the child process id, if available.
    ///
    /// `SandboxedChild` does not expose PID; returns `None` for now.
    pub(crate) fn pid(&self) -> Option<u32> {
        None
    }

    /// Waits for the sandboxed process to exit, returning its exit code.
    ///
    /// Sandbox violations are logged but treated as a normal (non-zero) exit.
    pub(crate) async fn wait(mut self) -> Option<i32> {
        match self.child.wait().await {
            Ok(status) => status.code(),
            Err(SandboxError::ExecutionViolation(err)) => {
                warn!(
                    violations = err.violations.len(),
                    "sandbox.host: process exited with sandbox violations"
                );
                err.status.and_then(|s| s.code())
            }
            Err(e) => {
                warn!(error = %e, "sandbox.host: error waiting for sandboxed process");
                None
            }
        }
    }
}

/// WASM guest execution runtime.
#[async_trait]
pub(crate) trait WasmSandbox: Send + Sync {
    /// Runs a WASM guest request to completion.
    async fn run(&self, request: RunWasmRequest) -> Result<WasmRunResult, FrameworkError>;
}

/// Host command sandbox runtime.
#[async_trait]
pub(crate) trait HostSandbox: Send + Sync {
    /// Runs a host command synchronously inside the sandbox.
    async fn run(&self, request: RunHostCommandRequest) -> Result<HostRunResult, FrameworkError>;

    /// Prepares a host command for later background execution.
    async fn prepare_spawn(
        &self,
        request: SpawnHostCommandRequest,
    ) -> Result<PreparedHostCommand, FrameworkError>;
}
