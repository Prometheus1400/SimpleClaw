use async_trait::async_trait;
use std::sync::Arc;

use crate::config::SummonToolConfig;
use crate::error::FrameworkError;
use crate::tools::{AgentInvokeRequest, Tool, ToolExecEnv, ToolExecutionOutcome, ToolRunOutput};

use super::common::parse_summon_args;

const SUMMON_DESCRIPTION_WITH_BG: &str =
    "Hand off to another agent using JSON: {agent, summary?, background?}";
const SUMMON_DESCRIPTION_SYNC_ONLY: &str =
    "Synchronously hand off to another agent with JSON: {agent, summary?}";
const SUMMON_SCHEMA_WITH_BG: &str =
    "{\"type\":\"object\",\"properties\":{\"agent\":{\"type\":\"string\"},\"summary\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"agent\"]}";
const SUMMON_SCHEMA_SYNC_ONLY: &str =
    "{\"type\":\"object\",\"properties\":{\"agent\":{\"type\":\"string\"},\"summary\":{\"type\":\"string\"}},\"required\":[\"agent\"]}";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummonTool {
    config: SummonToolConfig,
}

#[async_trait]
impl Tool for SummonTool {
    fn name(&self) -> &'static str {
        "summon"
    }

    fn description(&self) -> &'static str {
        if self.config.allow_background {
            SUMMON_DESCRIPTION_WITH_BG
        } else {
            SUMMON_DESCRIPTION_SYNC_ONLY
        }
    }

    fn input_schema_json(&self) -> &'static str {
        if self.config.allow_background {
            SUMMON_SCHEMA_WITH_BG
        } else {
            SUMMON_SCHEMA_SYNC_ONLY
        }
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.summon config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        self.execute_with_trace(ctx, args_json, session_id).await
    }

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_summon_args(args_json);
        if !self.target_allowed(&args.agent) {
            return Err(FrameworkError::Tool(format!(
                "summon target '{}' is not allowed by tools.summon.allowed",
                args.agent
            )));
        }
        let handoff = if args.summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{}`. Continue from session context and produce a final answer.",
                args.agent
            )
        } else {
            format!(
                "You were summoned as agent `{}` with handoff summary:\n{}",
                args.agent, args.summary
            )
        };

        if args.background {
            if !self.config.allow_background {
                return Err(FrameworkError::Tool(
                    "summon background mode is disabled by tools.summon.allow_background"
                        .to_owned(),
                ));
            }
            if !ctx.allow_async_tools {
                return Err(FrameworkError::Tool(
                    "background async tools are not allowed in delegated runs".to_owned(),
                ));
            }
            let invoker = Arc::clone(&ctx.invoker);
            let request = AgentInvokeRequest {
                target_agent_id: args.agent,
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt: handoff.clone(),
            };
            let started = ctx
                .async_tool_runs
                .start_delegated(
                    "summon",
                    &handoff,
                    &ctx.agent_id,
                    session_id,
                    ctx.completion_tx.clone(),
                    ctx.completion_route.clone(),
                    async move {
                        invoker
                            .invoke_agent(request)
                            .await
                            .map(|outcome| outcome.reply)
                    },
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let outcome = ctx
            .invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: args.agent,
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt: handoff,
            })
            .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput {
            output: outcome.reply,
            nested_tool_calls: outcome.tool_calls,
        }))
    }
}

impl SummonTool {
    pub(crate) fn set_allow_background(&mut self, allow_background: bool) {
        self.config.allow_background = allow_background;
    }

    fn target_allowed(&self, target: &str) -> bool {
        self.config.allowed.iter().any(|allowed| allowed == target)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio::time::{Duration, sleep};

    use super::SummonTool;
    use crate::config::DatabaseConfig;
    use crate::error::FrameworkError;
    use crate::memory::MemoryStore;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, Tool, ToolExecEnv,
        ToolExecutionOutcome, WorkerInvokeRequest,
    };

    struct TestInvoker;

    #[async_trait]
    impl AgentInvoker for TestInvoker {
        async fn invoke_agent(
            &self,
            _request: AgentInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: "summoned result".to_owned(),
                tool_calls: Vec::new(),
            })
        }

        async fn invoke_worker(
            &self,
            _request: WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool("unexpected worker invoke".to_owned()))
        }
    }

    async fn test_ctx() -> ToolExecEnv {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("simpleclaw_summon_test_{nanos}"));
        std::fs::create_dir_all(&root).expect("temp summon test dir should be created");
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
            invoker: Arc::new(TestInvoker),
            gateway: None,
            completion_tx: None,
            completion_route: None,
            allow_async_tools: true,
        }
    }

    #[test]
    fn empty_allowlist_denies_any_target() {
        let tool = SummonTool::default();
        assert!(!tool.target_allowed("planner"));
        assert!(!tool.target_allowed("reviewer"));
    }

    #[test]
    fn non_empty_allowlist_restricts_target() {
        let mut tool = SummonTool::default();
        tool.config.allowed = vec!["planner".to_owned(), "researcher".to_owned()];
        assert!(tool.target_allowed("planner"));
        assert!(!tool.target_allowed("reviewer"));
    }

    #[tokio::test]
    async fn summon_background_starts_delegated_async_run() {
        let mut tool = SummonTool::default();
        tool.configure(serde_json::json!({
            "allowed": ["helper"],
            "allow_background": true
        }))
        .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(
                &ctx,
                r#"{"agent":"helper","summary":"investigate","background":true}"#,
                "sess-1",
            )
            .await
            .expect("background summon should succeed");
        let ToolExecutionOutcome::AsyncStarted(started) = output else {
            panic!("background summon should start async run");
        };
        let accepted: Value =
            serde_json::from_str(&started.accepted_output()).expect("accepted output should parse");
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(accepted["kind"], "delegated");

        for _ in 0..20 {
            let snapshot = ctx
                .async_tool_runs
                .get(&started.run_id)
                .await
                .expect("snapshot should exist");
            if snapshot.status != crate::tools::AsyncToolRunStatus::Running {
                assert_eq!(snapshot.status, crate::tools::AsyncToolRunStatus::Completed);
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("delegated summon run did not complete in time");
    }

    #[tokio::test]
    async fn summon_background_rejected_when_async_not_allowed_in_context() {
        let mut tool = SummonTool::default();
        tool.configure(serde_json::json!({
            "allowed": ["helper"],
            "allow_background": true
        }))
        .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.allow_async_tools = false;

        let err = tool
            .execute(
                &ctx,
                r#"{"agent":"helper","summary":"investigate","background":true}"#,
                "sess-1",
            )
            .await
            .err()
            .expect("background summon should fail");
        assert!(
            err.to_string()
                .contains("background async tools are not allowed in delegated runs")
        );
    }
}
