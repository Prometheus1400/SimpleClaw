use async_trait::async_trait;
use serde_json::json;
use tokio::time::Duration;

use crate::config::SandboxMode;
use crate::error::FrameworkError;
use crate::tools::{ProcessStatus, Tool, ToolCtx, wait_for_completion};

use super::common::{exec_shell_command, parse_exec_args, snapshot_to_json};

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
        "Run local shell commands using JSON: {command, background?, yield_ms?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"},\"yield_ms\":{\"type\":\"integer\",\"minimum\":0}},\"required\":[\"command\"]}"
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
            if ctx.sandbox == SandboxMode::Wasm {
                return Err(FrameworkError::Tool(
                    "background exec is not supported in wasm sandbox mode".to_owned(),
                ));
            }
            let session_id = ctx
                .process_manager
                .spawn(
                    args.command.trim(),
                    if ctx.sandbox == SandboxMode::Wasm {
                        Some(&ctx.workspace_root)
                    } else {
                        None
                    },
                )
                .await?;
            let wait_for = Duration::from_millis(args.yield_ms.min(120_000));
            let snapshot = wait_for_completion(&ctx.process_manager, &session_id, wait_for).await?;
            if snapshot.status == ProcessStatus::Running {
                return Ok(json!({"status":"running","sessionId": session_id}).to_string());
            }
            return Ok(snapshot_to_json(&snapshot).to_string());
        }

        let result = exec_shell_command(
            args.command.trim(),
            if ctx.sandbox == SandboxMode::Wasm {
                Some(&ctx.workspace_root)
            } else {
                None
            },
        )
        .await?;
        Ok(result.to_string())
    }
}
