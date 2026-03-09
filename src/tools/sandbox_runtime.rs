use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use sandbox_runtime::{FilesystemConfig, NetworkConfig, SandboxManager, SandboxRuntimeConfig};
use tokio::runtime::Builder;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::config::ToolSandboxConfig;
use crate::error::FrameworkError;

const SANDBOX_INIT_TIMEOUT_SECS: u64 = 15;
const SANDBOX_WRAP_TIMEOUT_SECS: u64 = 15;

pub(crate) struct PreparedSandboxCommand {
    wrapped_command: String,
    manager: Arc<SandboxManager>,
}

impl PreparedSandboxCommand {
    pub(crate) fn wrapped_command(&self) -> &str {
        &self.wrapped_command
    }

    pub(crate) fn into_parts(self) -> (String, Arc<SandboxManager>) {
        (self.wrapped_command, self.manager)
    }

    pub(crate) async fn cleanup(self) {
        self.manager.reset().await;
    }
}

pub async fn prepare_command_for_exec(
    user_command: &str,
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
) -> Result<PreparedSandboxCommand, FrameworkError> {
    let workspace = crate::tools::sandbox::normalize_workspace_root(workspace_root)?;
    let runtime_cfg = build_runtime_config(&workspace, sandbox);
    let manager = Arc::new(SandboxManager::new());
    let init_started = Instant::now();
    debug!(status = "started", "sandbox.init");

    run_manager_init_with_timeout(Arc::clone(&manager), runtime_cfg, SANDBOX_INIT_TIMEOUT_SECS)
        .await?;
    debug!(
        status = "completed",
        elapsed_ms = init_started.elapsed().as_millis() as u64,
        "sandbox.init"
    );

    let wrap_started = Instant::now();
    debug!(status = "started", "sandbox.wrap");
    let wrapped = run_manager_wrap_with_timeout(
        Arc::clone(&manager),
        user_command.to_owned(),
        SANDBOX_WRAP_TIMEOUT_SECS,
    )
    .await?;
    debug!(
        status = "completed",
        elapsed_ms = wrap_started.elapsed().as_millis() as u64,
        "sandbox.wrap"
    );

    Ok(PreparedSandboxCommand {
        wrapped_command: wrapped,
        manager,
    })
}

async fn run_manager_init_with_timeout(
    manager: Arc<SandboxManager>,
    runtime_cfg: SandboxRuntimeConfig,
    timeout_secs: u64,
) -> Result<(), FrameworkError> {
    let join = tokio::task::spawn_blocking(move || {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| FrameworkError::Tool(format!("failed to create sandbox runtime: {e}")))?;
        rt.block_on(async {
            manager.initialize(runtime_cfg).await.map_err(|e| {
                FrameworkError::Tool(format!("failed to initialize sandbox runtime: {e}"))
            })
        })
    });

    timeout(Duration::from_secs(timeout_secs), join)
        .await
        .map_err(|_| FrameworkError::Tool(format!("sandbox init timed out after {timeout_secs}s")))?
        .map_err(|e| FrameworkError::Tool(format!("sandbox runtime join failed: {e}")))?
}

async fn run_manager_wrap_with_timeout(
    manager: Arc<SandboxManager>,
    command: String,
    timeout_secs: u64,
) -> Result<String, FrameworkError> {
    let join = tokio::task::spawn_blocking(move || {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| FrameworkError::Tool(format!("failed to create sandbox runtime: {e}")))?;
        rt.block_on(async {
            manager
                .wrap_with_sandbox(&command, Some("/bin/bash"), None)
                .await
                .map_err(|e| {
                    FrameworkError::Tool(format!("failed to wrap command in sandbox runtime: {e}"))
                })
        })
    });

    timeout(Duration::from_secs(timeout_secs), join)
        .await
        .map_err(|_| FrameworkError::Tool(format!("sandbox wrap timed out after {timeout_secs}s")))?
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolSandboxConfig;
    use std::path::Path;

    #[test]
    fn build_runtime_config_disables_network_by_default() {
        let sandbox = ToolSandboxConfig::default();
        let cfg = build_runtime_config(Path::new("/tmp/work"), &sandbox);
        assert_eq!(
            cfg.network.allowed_domains,
            vec!["sandbox.invalid".to_owned()]
        );
    }

    #[test]
    fn build_runtime_config_allows_network_when_enabled() {
        let sandbox = ToolSandboxConfig {
            network_enabled: Some(true),
            ..Default::default()
        };
        let cfg = build_runtime_config(Path::new("/tmp/work"), &sandbox);
        assert!(cfg.network.allowed_domains.is_empty());
    }

    #[test]
    fn build_write_allow_paths_includes_workspace_tmp_and_extra() {
        let sandbox = ToolSandboxConfig {
            extra_writable_paths: vec!["/var/tmp/simpleclaw-extra".to_owned()],
            ..Default::default()
        };
        let paths = build_write_allow_paths(Path::new("/tmp/work"), &sandbox);
        assert_eq!(paths[0], "/tmp/work");
        assert_eq!(paths[1], "/tmp");
        assert_eq!(paths[2], "/var/tmp/simpleclaw-extra");
    }
}
