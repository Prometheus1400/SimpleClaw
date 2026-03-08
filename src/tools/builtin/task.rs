use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, WorkerInvokeRequest};

use super::common::parse_task_args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTool {
    Worker,
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

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let prompt = parse_task_args(args_json);
        ctx.invoker
            .invoke_worker(WorkerInvokeRequest {
                current_agent_id: ctx
                    .completion_route
                    .as_ref()
                    .map(|route| route.target_agent_id.clone())
                    .ok_or_else(|| {
                        FrameworkError::Tool("current agent context unavailable".to_owned())
                    })?,
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt,
            })
            .await
    }
}
