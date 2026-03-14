use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::memory::MemorizeResult;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome};

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
        "Store a durable long-term fact that persists across sessions. Use this to remember user preferences, project context, or learned constraints. Semantically similar existing facts are updated in place rather than duplicated."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"fact\":{\"type\":\"string\",\"description\":\"The fact to store, as a complete sentence.\"},\"kind\":{\"type\":\"string\",\"enum\":[\"general\",\"profile\",\"preferences\",\"project\",\"task\",\"constraint\"],\"description\":\"Category. Defaults to general.\"},\"importance\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5,\"description\":\"Priority from 1 (low) to 5 (critical). Defaults to 3.\"}},\"required\":[\"fact\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let (fact, kind, importance) = parse_memorize_args(args_json);
        let result = ctx
            .memory
            .memorize(session_id, &fact, &kind, importance)
            .await?;
        let clamped = importance.clamp(1, 5);
        match result {
            MemorizeResult::Duplicate => Ok(ToolExecutionOutcome::completed(format!(
                "already memorized long-term fact (kind={kind}, importance={clamped})"
            ))),
            MemorizeResult::Updated { superseded_content } => {
                let preview = if superseded_content.len() > 80 {
                    format!("{}…", &superseded_content[..80])
                } else {
                    superseded_content
                };
                Ok(ToolExecutionOutcome::completed(format!(
                    "updated existing long-term fact (kind={kind}, importance={clamped}); superseded: \"{preview}\""
                )))
            }
            MemorizeResult::Inserted => Ok(ToolExecutionOutcome::completed(format!(
                "memorized long-term fact (kind={kind}, importance={clamped})"
            ))),
        }
    }
}
