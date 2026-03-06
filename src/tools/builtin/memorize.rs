use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::common::parse_memorize_args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorizeTool {
    LongTermStore,
}

#[async_trait]
impl Tool for MemorizeTool {
    fn name(&self) -> &'static str {
        "memorize"
    }

    fn description(&self) -> &'static str {
        "Store durable long-term memory using JSON: {fact, kind?, importance?(1-5)}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"fact\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"importance\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5}},\"required\":[\"fact\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (fact, kind, importance) = parse_memorize_args(args_json);
        let inserted = ctx
            .memory
            .memorize(session_id, &fact, &kind, importance)
            .await?;
        if !inserted {
            return Ok(format!(
                "already memorized long-term fact (kind={kind}, importance={})",
                importance.clamp(1, 5)
            ));
        }
        Ok(format!(
            "memorized long-term fact (kind={kind}, importance={})",
            importance.clamp(1, 5)
        ))
    }
}
