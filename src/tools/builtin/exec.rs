use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::{ExecContainerConfig, SandboxMode};
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::common::{command_output_to_json, exec_shell_command, parse_exec_args};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecTool {
    ShellCommand,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        "Run local shell commands using JSON: {command, background?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"command\"]}"
    }

    fn sandbox_aware(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args = parse_exec_args(args_json);
        if args.command.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "exec requires a non-empty command".to_owned(),
            ));
        }

        if args.background {
            let session_id = if ctx.sandbox == SandboxMode::On {
                ctx.process_manager
                    .spawn_podman(
                        args.command.trim(),
                        &ctx.workspace_root,
                        &ctx.exec_container,
                    )
                    .await?
            } else {
                ctx.process_manager
                    .spawn(args.command.trim(), Some(&ctx.workspace_root))
                    .await?
            };
            if let (Some(tx), Some(route)) =
                (ctx.completion_tx.as_ref(), ctx.completion_route.as_ref())
            {
                ctx.process_manager.spawn_completion_watcher(
                    session_id.clone(),
                    tx.clone(),
                    route.clone(),
                );
            }
            return Ok(json!({"status":"backgrounded","sessionId": session_id}).to_string());
        }

        let result = if ctx.sandbox == SandboxMode::On {
            exec_with_podman(
                args.command.trim(),
                &ctx.workspace_root,
                &ctx.exec_container,
            )
            .await?
        } else {
            exec_shell_command(args.command.trim(), None).await?
        };
        Ok(result.to_string())
    }
}

async fn exec_with_podman(
    command: &str,
    workspace_root: &Path,
    cfg: &ExecContainerConfig,
) -> Result<serde_json::Value, FrameworkError> {
    ensure_podman_available().await?;
    ensure_sandbox_image(cfg).await?;

    let workspace = crate::tools::sandbox::normalize_workspace_root(workspace_root)?;
    let mount_arg = format!("type=bind,src={},target=/workspace", workspace.display());

    let mut runner = Command::new("podman");
    runner
        .arg("run")
        .arg("--rm")
        .arg("--entrypoint")
        .arg("/bin/sh")
        .arg("--workdir")
        .arg("/workspace")
        .arg("--mount")
        .arg(mount_arg)
        .arg("--memory")
        .arg(format!("{}m", cfg.memory_mb.max(64)))
        .arg("--cpus")
        .arg(cpus_flag_value(cfg.cpus_milli.max(100)))
        .arg("--pids-limit")
        .arg(cfg.pids_limit.max(64).to_string());
    if !cfg.network_enabled {
        runner.arg("--network").arg("none");
    }
    runner
        .arg(&cfg.image)
        .arg("-lc")
        .arg(format!("cd /workspace && {command}"));

    let output = timeout(
        Duration::from_secs(cfg.exec_timeout_secs.max(1)),
        runner.output(),
    )
    .await
    .map_err(|_| {
        FrameworkError::Tool(format!(
            "exec timed out after {}s in podman sandbox",
            cfg.exec_timeout_secs.max(1)
        ))
    })?
    .map_err(|e| FrameworkError::Tool(format!("exec failed to start podman: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(command_output_to_json(
        output.status.code().unwrap_or(-1),
        stdout.trim(),
        stderr.trim(),
    ))
}

pub(crate) async fn ensure_podman_available() -> Result<(), FrameworkError> {
    let output = timeout(
        Duration::from_secs(8),
        Command::new("podman").arg("--version").output(),
    )
    .await
    .map_err(|_| FrameworkError::Tool("podman check timed out".to_owned()))?
    .map_err(|e| FrameworkError::Tool(format!("podman is required but not available: {e}")))?;
    if !output.status.success() {
        return Err(FrameworkError::Tool(format!(
            "podman --version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

pub(crate) async fn ensure_sandbox_image(cfg: &ExecContainerConfig) -> Result<(), FrameworkError> {
    let exists = Command::new("podman")
        .arg("image")
        .arg("exists")
        .arg(&cfg.image)
        .output()
        .await
        .map_err(|e| FrameworkError::Tool(format!("failed checking podman image: {e}")))?;
    if exists.status.success() {
        return Ok(());
    }

    let containerfile = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("sandbox")
        .join("podman")
        .join("Containerfile");
    let context_dir = containerfile
        .parent()
        .ok_or_else(|| FrameworkError::Tool("invalid sandbox containerfile path".to_owned()))?;
    if !containerfile.is_file() {
        return Err(FrameworkError::Tool(format!(
            "missing podman sandbox Containerfile: {}",
            containerfile.display()
        )));
    }

    let output = timeout(
        Duration::from_secs(cfg.build_timeout_secs.max(1)),
        Command::new("podman")
            .arg("build")
            .arg("-f")
            .arg(&containerfile)
            .arg("-t")
            .arg(&cfg.image)
            .arg(context_dir)
            .output(),
    )
    .await
    .map_err(|_| {
        FrameworkError::Tool(format!(
            "podman sandbox image build timed out after {}s",
            cfg.build_timeout_secs.max(1)
        ))
    })?
    .map_err(|e| FrameworkError::Tool(format!("failed to run podman build: {e}")))?;

    if !output.status.success() {
        return Err(FrameworkError::Tool(format!(
            "podman image build failed for {}: {}",
            cfg.image,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(())
}

pub(crate) fn cpus_flag_value(cpus_milli: u32) -> String {
    let whole = cpus_milli / 1000;
    let frac = cpus_milli % 1000;
    if frac == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac:03}")
    }
}
