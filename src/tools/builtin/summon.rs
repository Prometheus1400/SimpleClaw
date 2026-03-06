use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::common::parse_summon_args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonTool {
    Handoff,
}

#[async_trait]
impl Tool for SummonTool {
    fn name(&self) -> &'static str {
        "summon"
    }

    fn description(&self) -> &'static str {
        "Synchronously hand off to another agent with JSON: {agent, summary?}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"agent\":{\"type\":\"string\"},\"summary\":{\"type\":\"string\"}},\"required\":[\"agent\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (target, summary) = parse_summon_args(args_json);
        let service = ctx
            .summon_service
            .as_ref()
            .ok_or_else(|| FrameworkError::Tool("summon service unavailable".to_owned()))?;
        service.summon(&target, &summary, session_id).await
    }
}
