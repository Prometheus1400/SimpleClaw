use async_trait::async_trait;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use sandbox_runtime::{FilesystemConfig, NetworkConfig, SandboxManager, SandboxRuntimeConfig};
use tokio::process::Command;
use tokio::runtime::Builder;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::error::FrameworkError;
use crate::sandbox::{
    HostRunResult, HostSandbox, PreparedHostCommand, RunHostCommandRequest, SandboxPolicy,
    SpawnHostCommandRequest, normalize_workspace_root,
};

const SANDBOX_INIT_TIMEOUT_SECS: u64 = 15;
const SANDBOX_WRAP_TIMEOUT_SECS: u64 = 15;

pub(crate) struct DefaultHostSandbox;

#[async_trait]
impl HostSandbox for DefaultHostSandbox {
    async fn run(&self, request: RunHostCommandRequest) -> Result<HostRunResult, FrameworkError> {
        debug!(status = "started", phase = "host_run", "sandbox.host");
        let prepared =
            prepare_command_for_exec(&request.command, &request.workspace_root, &request.policy)
                .await?;
        let mut runner = Command::new("bash");
        runner.arg("-lc").arg(prepared.wrapped_command());
        runner.envs(&request.env);
        runner.current_dir(prepared.normalized_workspace_root());
        runner.kill_on_drop(true);

        let output_result = timeout(
            Duration::from_secs(request.timeout_seconds),
            runner.output(),
        )
        .await;
        prepared.cleanup().await;
        let output = match output_result {
            Ok(Ok(output)) => {
                debug!(status = "completed", phase = "host_run", "sandbox.host");
                output
            }
            Ok(Err(e)) => {
                debug!(
                    status = "failed",
                    phase = "host_run",
                    error = %e,
                    "sandbox.host"
                );
                return Err(FrameworkError::Tool(format!(
                    "exec failed to start sandbox runtime: {e}"
                )));
            }
            Err(_) => {
                debug!(
                    status = "timed_out",
                    phase = "host_run",
                    timeout_seconds = request.timeout_seconds,
                    "sandbox.host"
                );
                return Err(FrameworkError::Tool(format!(
                    "exec timed out after {}s in sandbox runtime",
                    request.timeout_seconds
                )));
            }
        };

        Ok(HostRunResult {
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    async fn prepare_spawn(
        &self,
        request: SpawnHostCommandRequest,
    ) -> Result<PreparedHostCommand, FrameworkError> {
        prepare_command_for_exec(&request.command, &request.workspace_root, &request.policy).await
    }
}

async fn prepare_command_for_exec(
    user_command: &str,
    workspace_root: &Path,
    sandbox: &SandboxPolicy,
) -> Result<PreparedHostCommand, FrameworkError> {
    let workspace = normalize_workspace_root(workspace_root)?;
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
        workspace.clone(),
        SANDBOX_WRAP_TIMEOUT_SECS,
    )
    .await?;
    debug!(
        status = "completed",
        elapsed_ms = wrap_started.elapsed().as_millis() as u64,
        "sandbox.wrap"
    );

    Ok(PreparedHostCommand {
        wrapped_command: wrapped,
        normalized_workspace_root: workspace,
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
    cwd: PathBuf,
    timeout_secs: u64,
) -> Result<String, FrameworkError> {
    let join = tokio::task::spawn_blocking(move || {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| FrameworkError::Tool(format!("failed to create sandbox runtime: {e}")))?;
        rt.block_on(async {
            manager
                .wrap_with_sandbox(&command, Some("/bin/bash"), None, &cwd)
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

fn build_runtime_config(workspace_root: &Path, sandbox: &SandboxPolicy) -> SandboxRuntimeConfig {
    let network = if sandbox.network_enabled {
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

fn build_write_allow_paths(workspace_root: &Path, sandbox: &SandboxPolicy) -> Vec<String> {
    let mut allow = vec![workspace_root.display().to_string(), "/tmp".to_owned()];
    allow.extend(sandbox.extra_writable_paths.iter().cloned());
    allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxPolicy;
    use std::path::Path;

    #[test]
    fn build_runtime_config_disables_network_by_default() {
        let sandbox = SandboxPolicy::default();
        let cfg = build_runtime_config(Path::new("/tmp/work"), &sandbox);
        assert_eq!(
            cfg.network.allowed_domains,
            vec!["sandbox.invalid".to_owned()]
        );
    }

    #[test]
    fn build_runtime_config_allows_network_when_enabled() {
        let sandbox = SandboxPolicy {
            network_enabled: true,
            ..Default::default()
        };
        let cfg = build_runtime_config(Path::new("/tmp/work"), &sandbox);
        assert!(cfg.network.allowed_domains.is_empty());
    }

    #[test]
    fn build_write_allow_paths_includes_workspace_tmp_and_extra() {
        let sandbox = SandboxPolicy {
            extra_writable_paths: vec!["/var/tmp/simpleclaw-extra".to_owned()],
            ..Default::default()
        };
        let paths = build_write_allow_paths(Path::new("/tmp/work"), &sandbox);
        assert_eq!(paths[0], "/tmp/work");
        assert_eq!(paths[1], "/tmp");
        assert_eq!(paths[2], "/var/tmp/simpleclaw-extra");
    }
}
