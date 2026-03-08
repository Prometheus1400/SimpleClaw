use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::memory::MemorizeResult;
use crate::tools::{Tool, ToolExecEnv};

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
        "Store durable long-term memory using JSON: {fact, kind?, importance?(1-5)}; kind one of: general|profile|preferences|project|task|constraint"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"fact\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"enum\":[\"general\",\"profile\",\"preferences\",\"project\",\"task\",\"constraint\"]},\"importance\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5}},\"required\":[\"fact\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (fact, kind, importance) = parse_memorize_args(args_json);
        let result = ctx
            .memory
            .memorize(session_id, &fact, &kind, importance)
            .await?;
        let clamped = importance.clamp(1, 5);
        match result {
            MemorizeResult::Duplicate => Ok(format!(
                "already memorized long-term fact (kind={kind}, importance={clamped})"
            )),
            MemorizeResult::Updated { superseded_content } => {
                let preview = if superseded_content.len() > 80 {
                    format!("{}…", &superseded_content[..80])
                } else {
                    superseded_content
                };
                Ok(format!(
                    "updated existing long-term fact (kind={kind}, importance={clamped}); superseded: \"{preview}\""
                ))
            }
            MemorizeResult::Inserted => Ok(format!(
                "memorized long-term fact (kind={kind}, importance={clamped})"
            )),
        }
    }
}
