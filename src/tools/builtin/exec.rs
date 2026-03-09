use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::{ExecToolConfig, ToolSandboxConfig};
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, sandbox_runtime};

use super::common::{command_output_to_json, exec_shell_command, parse_exec_args};

const DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecTool {
    config: ExecToolConfig,
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

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.exec config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args = parse_exec_args(args_json);
        if args.command.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "exec requires a non-empty command".to_owned(),
            ));
        }
        if args.background && !self.config.allow_background {
            return Err(FrameworkError::Tool(
                "exec background mode is disabled by tools.exec.allow_background".to_owned(),
            ));
        }

        if args.background {
            let (session_id, handle) = if self.config.sandbox.enabled {
                ctx.process_manager
                    .spawn_sandboxed(
                        args.command.trim(),
                        &ctx.workspace_root,
                        &self.config.sandbox,
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
                    handle,
                    tx.clone(),
                    route.clone(),
                );
            }
            return Ok(json!({"status":"backgrounded","sessionId": session_id}).to_string());
        }

        let result = if self.config.sandbox.enabled {
            exec_with_sandbox_runtime(
                args.command.trim(),
                &ctx.workspace_root,
                &self.config.sandbox,
                self.config
                    .timeout_seconds
                    .unwrap_or(DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS),
            )
            .await?
        } else {
            exec_shell_command(args.command.trim(), None).await?
        };
        Ok(result.to_string())
    }
}

async fn exec_with_sandbox_runtime(
    command: &str,
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
    timeout_seconds: u64,
) -> Result<serde_json::Value, FrameworkError> {
    let wrapped = sandbox_runtime::wrap_command_for_exec(command, workspace_root, sandbox).await?;
    let mut runner = Command::new("bash");
    runner.arg("-lc").arg(wrapped);
    runner.current_dir(crate::tools::sandbox::normalize_workspace_root(
        workspace_root,
    )?);

    let output = timeout(Duration::from_secs(timeout_seconds), runner.output())
        .await
        .map_err(|_| {
            FrameworkError::Tool(format!(
                "exec timed out after {timeout_seconds}s in sandbox runtime"
            ))
        })?
        .map_err(|e| FrameworkError::Tool(format!("exec failed to start sandbox runtime: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(command_output_to_json(
        output.status.code().unwrap_or(-1),
        stdout.trim(),
        stderr.trim(),
    ))
}
