//! Tool-agnostic sandbox runtime interfaces and implementations.

mod host;
mod wasm;

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
}

/// Prepared host command plus sandbox manager state for later cleanup.
pub(crate) struct PreparedHostCommand {
    wrapped_command: String,
    normalized_workspace_root: PathBuf,
    manager: Arc<sandbox_runtime::SandboxManager>,
}

/// Cleanup handle for a sandboxed host process.
pub(crate) struct HostSandboxCleanup {
    manager: Arc<sandbox_runtime::SandboxManager>,
}

/// Spawned sandboxed host process plus its cleanup handle.
pub(crate) struct SpawnedHostCommand {
    child: std::process::Child,
    cleanup: HostSandboxCleanup,
}

impl PreparedHostCommand {
    /// Returns the wrapped shell command.
    pub(crate) fn wrapped_command(&self) -> &str {
        &self.wrapped_command
    }

    /// Returns the normalized workspace root to execute from.
    pub(crate) fn normalized_workspace_root(&self) -> &std::path::Path {
        &self.normalized_workspace_root
    }

    /// Resets the sandbox manager and frees associated runtime state.
    pub(crate) async fn cleanup(self) {
        self.manager.reset().await;
    }

    /// Spawns the prepared command with caller-provided stdio handles.
    pub(crate) fn spawn(
        self,
        env: &BTreeMap<String, String>,
        stdout: std::process::Stdio,
        stderr: std::process::Stdio,
    ) -> Result<SpawnedHostCommand, FrameworkError> {
        let child = std::process::Command::new("bash")
            .arg("-lc")
            .arg(&self.wrapped_command)
            .envs(env)
            .current_dir(&self.normalized_workspace_root)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| {
                FrameworkError::Tool(format!("exec failed to start sandbox runtime: {e}"))
            })?;
        Ok(SpawnedHostCommand {
            child,
            cleanup: HostSandboxCleanup {
                manager: self.manager,
            },
        })
    }
}

impl HostSandboxCleanup {
    /// Resets the sandbox manager and frees associated runtime state.
    pub(crate) async fn cleanup(self) {
        self.manager.reset().await;
    }
}

impl SpawnedHostCommand {
    /// Returns the child process id, if available.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Consumes the spawned command into its owned parts.
    pub(crate) fn into_parts(self) -> (std::process::Child, HostSandboxCleanup) {
        (self.child, self.cleanup)
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
