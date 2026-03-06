use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

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
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let prompt = parse_task_args(args_json);
        let service = ctx
            .task_service
            .as_ref()
            .ok_or_else(|| FrameworkError::Tool("task service unavailable".to_owned()))?;
        service.run_task(&prompt, session_id).await
    }
}
