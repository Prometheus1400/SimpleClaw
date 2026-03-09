use std::path::Path;

use sandbox_runtime::{FilesystemConfig, NetworkConfig, SandboxManager, SandboxRuntimeConfig};
use tokio::runtime::Builder;

use crate::config::ToolSandboxConfig;
use crate::error::FrameworkError;

pub async fn wrap_command_for_exec(
    user_command: &str,
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
) -> Result<String, FrameworkError> {
    let workspace = crate::tools::sandbox::normalize_workspace_root(workspace_root)?;
    let command = user_command.to_owned();
    let sandbox_cfg = sandbox.clone();
    let join = tokio::task::spawn_blocking(move || {
        wrap_command_for_exec_blocking(&command, &workspace, &sandbox_cfg)
    });
    join.await
        .map_err(|e| FrameworkError::Tool(format!("sandbox runtime join failed: {e}")))?
}

fn build_runtime_config(
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
) -> SandboxRuntimeConfig {
    let network_enabled = sandbox.network_enabled.unwrap_or(false);
    let network = if network_enabled {
        NetworkConfig::default()
    } else {
        NetworkConfig {
            allowed_domains: vec!["sandbox.invalid".to_owned()],
            ..Default::default()
        }
    };
    let filesystem = FilesystemConfig {
        deny_read: Vec::new(),
        allow_write: build_write_allow_paths(workspace_root, sandbox),
        deny_write: Vec::new(),
        allow_git_config: Some(false),
    };
    SandboxRuntimeConfig {
        network,
        filesystem,
        allow_pty: Some(false),
        ..Default::default()
    }
}

fn build_write_allow_paths(workspace_root: &Path, sandbox: &ToolSandboxConfig) -> Vec<String> {
    let mut allow = vec![workspace_root.display().to_string(), "/tmp".to_owned()];
    allow.extend(sandbox.extra_writable_paths.iter().cloned());
    allow
}

fn wrap_command_for_exec_blocking(
    user_command: &str,
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
) -> Result<String, FrameworkError> {
    let runtime_cfg = build_runtime_config(workspace_root, sandbox);
    let manager = SandboxManager::new();
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| FrameworkError::Tool(format!("failed to create sandbox runtime: {e}")))?;
    rt.block_on(async {
        manager.initialize(runtime_cfg).await.map_err(|e| {
            FrameworkError::Tool(format!("failed to initialize sandbox runtime: {e}"))
        })?;
        manager
            .wrap_with_sandbox(user_command, Some("/bin/bash"), None)
            .await
            .map_err(|e| {
                FrameworkError::Tool(format!("failed to wrap command in sandbox runtime: {e}"))
            })
    })
}
