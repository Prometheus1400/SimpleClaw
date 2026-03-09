use async_trait::async_trait;

use crate::config::TaskToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolRunOutput, WorkerInvokeRequest};

use super::common::parse_task_args;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskTool {
    config: TaskToolConfig,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Run a delegated worker task using JSON: {prompt}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"prompt\":{\"type\":\"string\"}},\"required\":[\"prompt\"]}"
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.task config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        self.execute_with_trace(ctx, args_json, session_id)
            .await
            .map(|result| result.output)
    }

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let prompt = parse_task_args(args_json);
        let outcome = ctx
            .invoker
            .invoke_worker(WorkerInvokeRequest {
                current_agent_id: ctx.agent_id.clone(),
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt,
                max_steps_override: self.config.worker_max_steps,
            })
            .await?;
        Ok(ToolRunOutput {
            output: outcome.reply,
            nested_tool_calls: outcome.tool_calls,
        })
    }
}
