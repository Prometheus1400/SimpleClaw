use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::debug;

use crate::config::{ExecToolConfig, ToolSandboxConfig};
use crate::error::FrameworkError;
use crate::tools::{
    HostSandboxCommandRequest, HostSandboxRuntime, Tool, ToolExecEnv, ToolExecutionKind,
    ToolRunOutput, sandbox_runtime,
};

use super::common::{command_output_to_json, exec_shell_command, parse_exec_args};

const DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecTool {
    config: ExecToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPlan {
    pub command: String,
    pub background: bool,
    pub timeout_seconds: u64,
    pub env: std::collections::BTreeMap<String, String>,
    pub workspace_root: std::path::PathBuf,
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

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::HostSandbox]
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
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan)
            .await
            .map(|output| output.output)
    }
}

impl ExecTool {
    pub fn plan(&self, ctx: &ToolExecEnv, args_json: &str) -> Result<ExecPlan, FrameworkError> {
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
        Ok(ExecPlan {
            command: args.command.trim().to_owned(),
            background: args.background,
            timeout_seconds: self.config.timeout_seconds.unwrap_or(20),
            env: ctx.env.clone(),
            workspace_root: ctx.workspace_root.clone(),
        })
    }

    pub async fn execute_direct(
        &self,
        ctx: &ToolExecEnv,
        plan: ExecPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        if plan.background {
            let (session_id, handle) = ctx
                .process_manager
                .spawn(&plan.command, Some(&plan.workspace_root), &plan.env)
                .await?;
            ctx.process_manager.spawn_completion_watcher(
                session_id.clone(),
                handle,
                ctx.completion_tx.clone(),
                ctx.completion_route.clone(),
            );
            return Ok(ToolRunOutput::plain(
                json!({"status":"backgrounded","sessionId": session_id}).to_string(),
            ));
        }

        let result = exec_shell_command(
            &plan.command,
            Some(&plan.workspace_root),
            &plan.env,
            plan.timeout_seconds,
        )
        .await?;
        Ok(ToolRunOutput::plain(result.to_string()))
    }

    pub async fn execute_host_sandboxed(
        &self,
        ctx: &ToolExecEnv,
        plan: ExecPlan,
        runtime: &dyn HostSandboxRuntime,
    ) -> Result<ToolRunOutput, FrameworkError> {
        runtime
            .run_command(
                ctx,
                HostSandboxCommandRequest {
                    command: plan.command,
                    workspace_root: plan.workspace_root,
                    sandbox: self.config.sandbox.clone(),
                    env: plan.env,
                    timeout_seconds: self
                        .config
                        .timeout_seconds
                        .unwrap_or(DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS),
                    background: plan.background,
                },
            )
            .await
    }
}

pub(crate) async fn exec_with_sandbox_runtime(
    command: &str,
    workspace_root: &Path,
    sandbox: &ToolSandboxConfig,
    timeout_seconds: u64,
    env: &std::collections::BTreeMap<String, String>,
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
    runner.envs(env);
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
            env: std::collections::BTreeMap::new(),
            persona_root: PathBuf::from(&root),
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

        assert!(
            err.to_string()
                .contains("exec requires a non-empty command")
        );
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
            .execute(
                &ctx,
                "{\"command\":\"sleep 1\",\"background\":true}",
                "sess-1",
            )
            .await
            .err()
            .expect("background execution should fail");

        assert!(
            err.to_string()
                .contains("exec background mode is disabled by tools.exec.allow_background")
        );
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
    async fn exec_injects_configured_env_into_foreground_command() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.env.insert(
            "SIMPLECLAW_EXEC_TEST_TOKEN".to_owned(),
            "from-config".to_owned(),
        );

        let output = tool
            .execute(
                &ctx,
                r#"{"command":"printf %s \"$SIMPLECLAW_EXEC_TEST_TOKEN\""}"#,
                "sess-1",
            )
            .await
            .expect("foreground exec should succeed");
        let parsed: Value = serde_json::from_str(&output).expect("exec output should be json");

        assert_eq!(parsed["stdout"], "from-config");
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
            .execute(
                &ctx,
                r#"{"command":"sleep 0.1","background":true}"#,
                "sess-1",
            )
            .await
            .expect("background exec should succeed");
        let parsed: Value = serde_json::from_str(&output).expect("exec output should be json");

        assert_eq!(parsed["status"], "backgrounded");
        let session_id = parsed["sessionId"]
            .as_str()
            .expect("backgrounded response should include session id");
        let sessions = ctx.process_manager.list().await;
        assert!(
            sessions
                .iter()
                .any(|snapshot| snapshot.session_id == session_id)
        );
    }

    #[tokio::test]
    async fn exec_injects_configured_env_into_background_command() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.env.insert(
            "SIMPLECLAW_EXEC_BG_TOKEN".to_owned(),
            "background-value".to_owned(),
        );
        let output_path = ctx.workspace_root.join("bg-env.txt");

        let command = format!(
            "printf %s \"$SIMPLECLAW_EXEC_BG_TOKEN\" > {}",
            output_path.display()
        );
        tool.execute(
            &ctx,
            &serde_json::json!({ "command": command, "background": true }).to_string(),
            "sess-1",
        )
        .await
        .expect("background exec should succeed");

        for _ in 0..20 {
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                assert_eq!(content, "background-value");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        panic!("background exec did not write env output");
    }
}
