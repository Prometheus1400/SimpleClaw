use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome};

use super::common::{parse_background_args, snapshot_to_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTool {
    Lifecycle,
}

#[async_trait]
impl Tool for BackgroundTool {
    fn name(&self) -> &'static str {
        "background"
    }

    fn description(&self) -> &'static str {
        "Check on or manage background async tool runs. action=\"list\" shows all runs. action=\"status\" requires run_id, returns details of one run. action=\"kill\" requires run_id, terminates a run."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\",\"enum\":[\"list\",\"status\",\"kill\"],\"description\":\"Whether to list runs, inspect one run, or terminate one run.\"},\"run_id\":{\"type\":\"string\",\"description\":\"Required for status and kill actions.\"}},\"required\":[\"action\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_background_args(args_json);
        match args.action.as_str() {
            "list" => {
                let items = ctx
                    .async_tool_runs
                    .list_for_session(&ctx.agent_id, session_id)
                    .await;
                let payload = items
                    .into_iter()
                    .map(|snapshot| snapshot_to_json(&snapshot))
                    .collect::<Vec<_>>();
                Ok(ToolExecutionOutcome::completed(
                    json!({"status":"ok","runs": payload}).to_string(),
                ))
            }
            "status" => {
                let run_id = args.run_id.ok_or_else(|| {
                    FrameworkError::Tool("background status requires run_id".to_owned())
                })?;
                let snapshot = ctx
                    .async_tool_runs
                    .get_for_session(&run_id, &ctx.agent_id, session_id)
                    .await?;
                Ok(ToolExecutionOutcome::completed(
                    snapshot_to_json(&snapshot).to_string(),
                ))
            }
            "kill" => {
                let run_id = args.run_id.ok_or_else(|| {
                    FrameworkError::Tool("background kill requires run_id".to_owned())
                })?;
                let snapshot = ctx
                    .async_tool_runs
                    .kill_for_session(&run_id, &ctx.agent_id, session_id)
                    .await?;
                Ok(ToolExecutionOutcome::completed(
                    snapshot_to_json(&snapshot).to_string(),
                ))
            }
            other => Err(FrameworkError::Tool(format!(
                "background action must be one of list|status|kill, got: {other}"
            ))),
        }
    }
}
