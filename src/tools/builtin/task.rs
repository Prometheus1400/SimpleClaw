use async_trait::async_trait;
use std::sync::Arc;

use crate::config::TaskToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome, ToolRunOutput, WorkerInvokeRequest};

use super::common::parse_task_args;

const TASK_DESCRIPTION_WITH_BG: &str =
    "Run a delegated worker task using JSON: {prompt, background?}";
const TASK_DESCRIPTION_SYNC_ONLY: &str = "Run a delegated worker task using JSON: {prompt}";
const TASK_SCHEMA_WITH_BG: &str = "{\"type\":\"object\",\"properties\":{\"prompt\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"prompt\"]}";
const TASK_SCHEMA_SYNC_ONLY: &str = "{\"type\":\"object\",\"properties\":{\"prompt\":{\"type\":\"string\"}},\"required\":[\"prompt\"]}";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTool {
    config: TaskToolConfig,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use serde_json::Value;
    use tokio::time::{Duration, sleep};

    use super::TaskTool;
    use crate::approval::UnavailableApprovalRequester;
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
            Err(FrameworkError::Tool("unexpected agent invoke".to_owned()))
        }

        async fn invoke_worker(
            &self,
            _request: WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: "worker result".to_owned(),
                tool_calls: Vec::new(),
            })
        }
    }

    async fn test_ctx() -> ToolExecEnv<'static> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("simpleclaw_task_test_{nanos}"));
        std::fs::create_dir_all(&root).expect("temp task test dir should be created");
        let short = root.join("short.db");
        let long = root.join("long.db");
        let memory = MemoryStore::new_without_embedder(&short, &long, &DatabaseConfig::default())
            .await
            .expect("memory should initialize");
        let memory = Box::leak(Box::new(memory));
        let env = Box::leak(Box::new(std::collections::BTreeMap::new()));
        let persona_root = Box::leak(Box::new(PathBuf::from(&root)));
        let workspace_root = Box::leak(Box::new(PathBuf::from(&root)));
        let owner_ids = Box::leak(Box::new(vec!["user-1".to_owned()]));
        let async_tool_runs = Box::leak(Box::new(Arc::new(AsyncToolRunManager::new())));
        let invoker: &'static Arc<dyn AgentInvoker> =
            Box::leak(Box::new(Arc::new(TestInvoker) as Arc<dyn AgentInvoker>));
        ToolExecEnv {
            agent_id: "test-agent",
            agent_name: "Test Agent",
            memory,
            history_messages: 10,
            env,
            persona_root,
            workspace_root,
            user_id: "user-1",
            owner_ids,
            async_tool_runs,
            invoker,
            gateway: None,
            completion_tx: None,
            completion_route: None,
            allow_async_tools: true,
            approval_requester: Arc::new(UnavailableApprovalRequester),
        }
    }

    #[tokio::test]
    async fn task_background_starts_delegated_async_run() {
        let mut tool = TaskTool::default();
        tool.configure(serde_json::json!({"allow_background": true}))
            .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(&ctx, r#"{"prompt":"do work","background":true}"#, "sess-1")
            .await
            .expect("background task should succeed");
        let ToolExecutionOutcome::AsyncStarted(started) = output else {
            panic!("background task should start async run");
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
        panic!("delegated task run did not complete in time");
    }

    #[tokio::test]
    async fn task_background_rejected_when_async_not_allowed_in_context() {
        let mut tool = TaskTool::default();
        tool.configure(serde_json::json!({"allow_background": true}))
            .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.allow_async_tools = false;

        let err = tool
            .execute(&ctx, r#"{"prompt":"do work","background":true}"#, "sess-1")
            .await
            .err()
            .expect("background task should fail");
        assert!(
            err.to_string()
                .contains("background async tools are not allowed in delegated runs")
        );
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        if self.config.allow_background {
            TASK_DESCRIPTION_WITH_BG
        } else {
            TASK_DESCRIPTION_SYNC_ONLY
        }
    }

    fn input_schema_json(&self) -> &'static str {
        if self.config.allow_background {
            TASK_SCHEMA_WITH_BG
        } else {
            TASK_SCHEMA_SYNC_ONLY
        }
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.task config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        self.execute_with_trace(ctx, args_json, session_id).await
    }

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_task_args(args_json);

        if args.background {
            if !self.config.allow_background {
                return Err(FrameworkError::Tool(
                    "task background mode is disabled by tools.task.allow_background".to_owned(),
                ));
            }
            if !ctx.allow_async_tools {
                return Err(FrameworkError::Tool(
                    "background async tools are not allowed in delegated runs".to_owned(),
                ));
            }
            let invoker = Arc::clone(&ctx.invoker);
            let request = WorkerInvokeRequest {
                current_agent_id: ctx.agent_id.to_owned(),
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.to_owned(),
                prompt: args.prompt.clone(),
                max_steps_override: self.config.worker_max_steps,
                approval_requester: Arc::clone(&ctx.approval_requester),
            };
            let started = ctx
                .async_tool_runs
                .start_delegated(
                    "task",
                    &args.prompt,
                    &ctx.agent_id,
                    session_id,
                    ctx.completion_tx.cloned(),
                    ctx.completion_route.cloned(),
                    async move {
                        invoker
                            .invoke_worker(request)
                            .await
                            .map(|outcome| outcome.reply)
                    },
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let outcome = ctx
            .invoker
            .invoke_worker(WorkerInvokeRequest {
                current_agent_id: ctx.agent_id.to_owned(),
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.to_owned(),
                prompt: args.prompt,
                max_steps_override: self.config.worker_max_steps,
                approval_requester: Arc::clone(&ctx.approval_requester),
            })
            .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput {
            output: outcome.reply,
            nested_tool_calls: outcome.tool_calls,
        }))
    }
}

impl TaskTool {
    pub(crate) fn set_allow_background(&mut self, allow_background: bool) {
        self.config.allow_background = allow_background;
    }
}
