use async_trait::async_trait;
use std::path::Path;

use sandbox_runtime::{
    FilesystemConfig, NetworkConfig, SandboxError, SandboxRuntimeConfig, SandboxedCommand,
};
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

use crate::error::FrameworkError;
use crate::sandbox::{
    HostRunResult, HostSandbox, PreparedHostCommand, RunHostCommandRequest, SandboxPolicy,
    SpawnHostCommandRequest, normalize_workspace_root,
};

pub(crate) struct DefaultHostSandbox;

#[async_trait]
impl HostSandbox for DefaultHostSandbox {
    async fn run(&self, request: RunHostCommandRequest) -> Result<HostRunResult, FrameworkError> {
        debug!(status = "started", phase = "host_run", "sandbox.host");

        let workspace = normalize_workspace_root(&request.workspace_root)?;
        let runtime_cfg = build_runtime_config(&workspace, &request.policy);

        let result = timeout(
            Duration::from_secs(request.timeout_seconds),
            SandboxedCommand::new("bash")
                .arg("-lc")
                .arg(&request.command)
                .envs(&request.env)
                .current_dir(&workspace)
                .config(runtime_cfg)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                debug!(status = "completed", phase = "host_run", "sandbox.host");
                Ok(HostRunResult {
                    stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                    exit_code: output.status.code().unwrap_or(-1),
                    sandbox_violated: false,
                })
            }
            Ok(Err(SandboxError::ExecutionViolation(err))) => {
                warn!(
                    violations = err.violations.len(),
                    phase = "host_run",
                    "sandbox.host: execution violated sandbox policy"
                );
                Ok(HostRunResult {
                    stdout: String::from_utf8_lossy(&err.stdout).trim().to_owned(),
                    stderr: String::from_utf8_lossy(&err.stderr).trim().to_owned(),
                    exit_code: err.status.and_then(|s| s.code()).unwrap_or(-1),
                    sandbox_violated: true,
                })
            }
            Ok(Err(e)) => {
                debug!(
                    status = "failed",
                    phase = "host_run",
                    error = %e,
                    "sandbox.host"
                );
                Err(FrameworkError::Tool(format!(
                    "exec failed in sandbox runtime: {e}"
                )))
            }
            Err(_) => {
                debug!(
                    status = "timed_out",
                    phase = "host_run",
                    timeout_seconds = request.timeout_seconds,
                    "sandbox.host"
                );
                Err(FrameworkError::Tool(format!(
                    "exec timed out after {}s in sandbox runtime",
                    request.timeout_seconds
                )))
            }
        }
    }

    async fn prepare_spawn(
        &self,
        request: SpawnHostCommandRequest,
    ) -> Result<PreparedHostCommand, FrameworkError> {
        let workspace = normalize_workspace_root(&request.workspace_root)?;
        let runtime_cfg = build_runtime_config(&workspace, &request.policy);

        let mut command = SandboxedCommand::new("bash");
        command
            .arg("-lc")
            .arg(&request.command)
            .current_dir(&workspace)
            .config(runtime_cfg);

        Ok(PreparedHostCommand { command })
    }
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
        allow_read: Vec::new(),
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
        assert_eq!(paths.len(), 3);
    }
}
