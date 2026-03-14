use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome};

use super::common::{parse_wait_args, snapshot_to_json};

#[derive(Debug, Clone, Copy)]
pub struct WaitTool;

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        "Block until one or more background async tool runs complete or a timeout expires. Returns snapshots for the requested runs."
    }

    fn input_schema_json(&self) -> &str {
        "{\"type\":\"object\",\"properties\":{\"run_ids\":{\"type\":\"array\",\"items\":{\"type\":\"string\"},\"description\":\"Background run IDs to wait on.\"},\"timeout_ms\":{\"type\":\"integer\",\"description\":\"Optional timeout in milliseconds. Defaults to 30000 and clamps to 120000.\"}},\"required\":[\"run_ids\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_wait_args(args_json);
        if args.run_ids.is_empty() {
            return Err(FrameworkError::Tool(
                "wait requires at least one run_id".to_owned(),
            ));
        }

        let snapshots = ctx
            .async_tool_runs
            .wait_for_session(&args.run_ids, ctx.agent_id, session_id, args.timeout_ms)
            .await?;
        let runs = snapshots.iter().map(snapshot_to_json).collect::<Vec<_>>();

        Ok(ToolExecutionOutcome::completed(
            json!({ "status": "ok", "runs": runs }).to_string(),
        ))
    }
}
