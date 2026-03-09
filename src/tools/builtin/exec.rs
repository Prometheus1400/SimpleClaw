use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

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
            ctx.process_manager.spawn_completion_watcher(
                session_id.clone(),
                handle,
                ctx.completion_tx.clone(),
                ctx.completion_route.clone(),
            );
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
    debug!(
        status = "started",
        tool = "exec",
        phase = "sandbox_exec",
        "sandbox.exec"
    );
    let prepared =
        sandbox_runtime::prepare_command_for_exec(command, workspace_root, sandbox).await?;
    let mut runner = Command::new("bash");
    runner.arg("-lc").arg(prepared.wrapped_command());
    runner.current_dir(crate::tools::sandbox::normalize_workspace_root(
        workspace_root,
    )?);
    runner.kill_on_drop(true);

    let output_result = timeout(Duration::from_secs(timeout_seconds), runner.output()).await;
    prepared.cleanup().await;
    let output = match output_result {
        Ok(Ok(output)) => {
            debug!(
                status = "completed",
                tool = "exec",
                phase = "sandbox_exec",
                "sandbox.exec"
            );
            output
        }
        Ok(Err(e)) => {
            debug!(
                status = "failed",
                tool = "exec",
                phase = "sandbox_exec",
                error = %e,
                "sandbox.exec"
            );
            return Err(FrameworkError::Tool(format!(
                "exec failed to start sandbox runtime: {e}"
            )));
        }
        Err(_) => {
            debug!(
                status = "timed_out",
                tool = "exec",
                phase = "sandbox_exec",
                timeout_seconds,
                "sandbox.exec"
            );
            return Err(FrameworkError::Tool(format!(
                "exec timed out after {timeout_seconds}s in sandbox runtime"
            )));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(command_output_to_json(
        output.status.code().unwrap_or(-1),
        stdout.trim(),
        stderr.trim(),
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use serde_json::Value;

    use super::ExecTool;
    use crate::config::DatabaseConfig;
    use crate::error::FrameworkError;
    use crate::memory::MemoryStore;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, InvokeOutcome, ProcessManager, Tool, ToolExecEnv,
        WorkerInvokeRequest,
    };

    struct NoopInvoker;

    #[async_trait]
    impl AgentInvoker for NoopInvoker {
        async fn invoke_agent(
            &self,
            _request: AgentInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: String::new(),
                tool_calls: Vec::new(),
            })
        }

        async fn invoke_worker(
            &self,
            _request: WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: String::new(),
                tool_calls: Vec::new(),
            })
        }
    }

    async fn test_ctx() -> ToolExecEnv {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("simpleclaw_exec_test_{nanos}"));
        std::fs::create_dir_all(&root).expect("temp exec test dir should be created");
        let short = root.join("short.db");
        let long = root.join("long.db");
        let memory = MemoryStore::new_without_embedder(&short, &long, &DatabaseConfig::default())
            .await
            .expect("memory should initialize");
        ToolExecEnv {
            agent_id: "test-agent".to_owned(),
            memory: Arc::new(memory),
            history_messages: 10,
            workspace_root: PathBuf::from(&root),
            user_id: "user-1".to_owned(),
            owner_ids: vec!["user-1".to_owned()],
            process_manager: Arc::new(ProcessManager::new()),
            invoker: Arc::new(NoopInvoker),
            gateway: None,
            completion_tx: None,
            completion_route: None,
        }
    }

    #[tokio::test]
    async fn exec_rejects_empty_command() {
        let tool = ExecTool::default();
        let ctx = test_ctx().await;

        let err = tool
            .execute(&ctx, r#"{"command":"   "}"#, "sess-1")
            .await
            .err()
            .expect("empty command should fail");

        assert!(err.to_string().contains("exec requires a non-empty command"));
    }

    #[tokio::test]
    async fn exec_rejects_background_when_disabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": false,
            "sandbox": { "enabled": false }
        }))
            .expect("config should apply");
        let ctx = test_ctx().await;

        let err = tool
            .execute(&ctx, "{\"command\":\"sleep 1\",\"background\":true}", "sess-1")
            .await
            .err()
            .expect("background execution should fail");

        assert!(err
            .to_string()
            .contains("exec background mode is disabled by tools.exec.allow_background"));
    }

    #[tokio::test]
    async fn exec_runs_foreground_command_without_sandbox() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(&ctx, r#"{"command":"printf hello"}"#, "sess-1")
            .await
            .expect("foreground exec should succeed");
        let parsed: Value = serde_json::from_str(&output).expect("exec output should be json");

        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["exitCode"], 0);
        assert_eq!(parsed["stdout"], "hello");
        assert_eq!(parsed["stderr"], "");
    }

    #[tokio::test]
    async fn exec_backgrounds_process_when_enabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(&ctx, r#"{"command":"sleep 0.1","background":true}"#, "sess-1")
            .await
            .expect("background exec should succeed");
        let parsed: Value = serde_json::from_str(&output).expect("exec output should be json");

        assert_eq!(parsed["status"], "backgrounded");
        let session_id = parsed["sessionId"]
            .as_str()
            .expect("backgrounded response should include session id");
        let sessions = ctx.process_manager.list().await;
        assert!(sessions.iter().any(|snapshot| snapshot.session_id == session_id));
    }
}
