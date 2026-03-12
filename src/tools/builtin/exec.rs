use async_trait::async_trait;

use crate::config::ExecToolConfig;
use crate::error::FrameworkError;
use crate::sandbox::{HostSandbox, RunHostCommandRequest, SandboxPolicy, SpawnHostCommandRequest};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::common::{command_output_to_json, exec_shell_command, parse_exec_args};
use super::read::resolve_path_for_read;

const DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS: u64 = 120;
const EXEC_DESCRIPTION_WITH_BG: &str =
    "Run local shell commands using JSON: {command, workdir?, background?}. Returns JSON string.";
const EXEC_DESCRIPTION_SYNC_ONLY: &str =
    "Run local shell commands using JSON: {command, workdir?}. Returns JSON string.";
const EXEC_SCHEMA_WITH_BG: &str = "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"command\"]}";
const EXEC_SCHEMA_SYNC_ONLY: &str = "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"}},\"required\":[\"command\"]}";

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
    pub workdir: std::path::PathBuf,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        if self.config.allow_background {
            EXEC_DESCRIPTION_WITH_BG
        } else {
            EXEC_DESCRIPTION_SYNC_ONLY
        }
    }

    fn input_schema_json(&self) -> &'static str {
        if self.config.allow_background {
            EXEC_SCHEMA_WITH_BG
        } else {
            EXEC_SCHEMA_SYNC_ONLY
        }
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
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan, session_id).await
    }
}

impl ExecTool {
    pub(crate) fn set_allow_background(&mut self, allow_background: bool) {
        self.config.allow_background = allow_background;
    }

    fn sandbox_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            network_enabled: self.config.sandbox.network_enabled.unwrap_or(false),
            extra_writable_paths: self.config.sandbox.extra_writable_paths.clone(),
        }
    }

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
        if args.background && !ctx.allow_async_tools {
            return Err(FrameworkError::Tool(
                "background async tools are not allowed in delegated runs".to_owned(),
            ));
        }
        let workdir = if let Some(workdir) = args.workdir.as_deref() {
            resolve_path_for_read(workdir, &ctx.workspace_root)?
        } else {
            ctx.workspace_root.clone()
        };
        if !workdir.is_dir() {
            return Err(FrameworkError::Tool(format!(
                "exec workdir must be a directory: {}",
                workdir.display()
            )));
        }
        Ok(ExecPlan {
            command: args.command.trim().to_owned(),
            background: args.background,
            timeout_seconds: self.config.timeout_seconds.unwrap_or(20),
            env: ctx.env.clone(),
            workdir,
        })
    }

    pub async fn execute_direct(
        &self,
        ctx: &ToolExecEnv,
        plan: ExecPlan,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        if plan.background {
            let started = ctx
                .async_tool_runs
                .start_process(
                    "exec",
                    &plan.command,
                    &ctx.agent_id,
                    session_id,
                    Some(&plan.workdir),
                    &plan.env,
                    ctx.completion_tx.clone(),
                    ctx.completion_route.clone(),
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let result = exec_shell_command(
            &plan.command,
            Some(&plan.workdir),
            &plan.env,
            plan.timeout_seconds,
        )
        .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput::plain(
            result.to_string(),
        )))
    }

    pub async fn execute_host_sandboxed(
        &self,
        ctx: &ToolExecEnv,
        plan: ExecPlan,
        session_id: &str,
        runtime: &dyn HostSandbox,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        if plan.background {
            let prepared = runtime
                .prepare_spawn(SpawnHostCommandRequest {
                    command: plan.command.clone(),
                    workspace_root: plan.workdir.clone(),
                    policy: self.sandbox_policy(),
                })
                .await?;
            let started = ctx
                .async_tool_runs
                .start_prepared_process(
                    "exec",
                    &plan.command,
                    &ctx.agent_id,
                    session_id,
                    prepared,
                    &plan.env,
                    ctx.completion_tx.clone(),
                    ctx.completion_route.clone(),
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let output = runtime
            .run(RunHostCommandRequest {
                command: plan.command,
                workspace_root: plan.workdir,
                policy: self.sandbox_policy(),
                env: plan.env,
                timeout_seconds: self
                    .config
                    .timeout_seconds
                    .unwrap_or(DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS),
            })
            .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput::plain(
            command_output_to_json(output.exit_code, &output.stdout, &output.stderr).to_string(),
        )))
    }
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
        AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, Tool, ToolExecEnv,
        ToolExecutionOutcome, WorkerInvokeRequest,
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
            async_tool_runs: Arc::new(AsyncToolRunManager::new()),
            invoker: Arc::new(NoopInvoker),
            gateway: None,
            completion_tx: None,
            completion_route: None,
            allow_async_tools: true,
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
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

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
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

        assert_eq!(parsed["stdout"], "from-config");
    }

    #[tokio::test]
    async fn exec_runs_in_overridden_workdir() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let ctx = test_ctx().await;
        let nested = ctx.workspace_root.join("nested");
        std::fs::create_dir_all(&nested).expect("nested directory should exist");

        let output = tool
            .execute(
                &ctx,
                &serde_json::json!({
                    "command": "pwd",
                    "workdir": nested.to_string_lossy(),
                })
                .to_string(),
                "sess-1",
            )
            .await
            .expect("foreground exec should succeed");
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

        let stdout = parsed["stdout"].as_str().expect("stdout should be string");
        assert!(stdout.ends_with("/nested"));
        assert!(stdout.contains("simpleclaw_exec_test_"));
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
        let ToolExecutionOutcome::AsyncStarted(output) = output else {
            panic!("background exec should start async tool run");
        };
        let parsed: Value =
            serde_json::from_str(&output.accepted_output()).expect("exec output should be json");

        assert_eq!(parsed["status"], "accepted");
        let run_id = parsed["runId"]
            .as_str()
            .expect("accepted response should include run id");
        let sessions = ctx
            .async_tool_runs
            .list_for_session(&ctx.agent_id, "sess-1")
            .await;
        assert!(sessions.iter().any(|snapshot| snapshot.run_id == run_id));
    }

    #[tokio::test]
    async fn exec_rejects_background_when_async_tools_disallowed_in_context() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.allow_async_tools = false;

        let err = tool
            .execute(
                &ctx,
                r#"{"command":"sleep 0.1","background":true}"#,
                "sess-1",
            )
            .await
            .err()
            .expect("background execution should fail");

        assert!(
            err.to_string()
                .contains("background async tools are not allowed in delegated runs")
        );
    }

    #[test]
    fn exec_schema_hides_background_when_disabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": false,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");

        assert_eq!(
            tool.input_schema_json(),
            "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"}},\"required\":[\"command\"]}"
        );
        assert_eq!(
            tool.description(),
            "Run local shell commands using JSON: {command, workdir?}. Returns JSON string."
        );
    }

    #[test]
    fn exec_schema_exposes_background_when_enabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");

        assert_eq!(
            tool.input_schema_json(),
            "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"command\"]}"
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
