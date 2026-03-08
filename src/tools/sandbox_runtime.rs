use std::path::Path;

use sandbox_runtime::{FilesystemConfig, NetworkConfig, SandboxManager, SandboxRuntimeConfig};

use crate::config::{AgentSandboxConfig, SandboxNetworkMode};
use crate::error::FrameworkError;

pub async fn wrap_command_for_exec(
    user_command: &str,
    workspace_root: &Path,
    sandbox: &AgentSandboxConfig,
) -> Result<String, FrameworkError> {
    let workspace = crate::tools::sandbox::normalize_workspace_root(workspace_root)?;
    let runtime_cfg = build_runtime_config(&workspace, sandbox);
    let manager = SandboxManager::new();
    manager
        .initialize(runtime_cfg)
        .await
        .map_err(|e| FrameworkError::Tool(format!("failed to initialize sandbox runtime: {e}")))?;
    manager
        .wrap_with_sandbox(user_command, Some("/bin/bash"), None)
        .await
        .map_err(|e| {
            FrameworkError::Tool(format!("failed to wrap command in sandbox runtime: {e}"))
        })
}

fn build_runtime_config(
    workspace_root: &Path,
    sandbox: &AgentSandboxConfig,
) -> SandboxRuntimeConfig {
    let network = if sandbox.network.mode == SandboxNetworkMode::Enabled {
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

fn build_write_allow_paths(workspace_root: &Path, sandbox: &AgentSandboxConfig) -> Vec<String> {
    let mut allow = vec![workspace_root.display().to_string(), "/tmp".to_owned()];
    allow.extend(sandbox.filesystem.extra_writable_paths.iter().cloned());
    allow
}
