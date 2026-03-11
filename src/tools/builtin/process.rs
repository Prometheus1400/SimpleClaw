use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{AsyncToolRunKind, Tool, ToolExecEnv, ToolExecutionOutcome};

use super::common::{parse_process_args, snapshot_to_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTool {
    Lifecycle,
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &'static str {
        "process"
    }

    fn description(&self) -> &'static str {
        "Manage exec background processes using JSON: {action: list|poll|kill|forget, session_id?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\",\"enum\":[\"list\",\"poll\",\"kill\",\"forget\"]},\"session_id\":{\"type\":\"string\"}},\"required\":[\"action\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let args = parse_process_args(args_json);
        match args.action.as_str() {
            "list" => {
                let items = ctx
                    .async_tool_runs
                    .list()
                    .await
                    .into_iter()
                    .filter(|snapshot| snapshot.kind == AsyncToolRunKind::Process)
                    .collect::<Vec<_>>();
                let payload = items
                    .into_iter()
                    .map(|snapshot| snapshot_to_json(&snapshot))
                    .collect::<Vec<_>>();
                Ok(ToolExecutionOutcome::completed(
                    json!({"status":"ok","processes": payload}).to_string(),
                ))
            }
            "poll" => {
                let run_id = args.run_id.ok_or_else(|| {
                    FrameworkError::Tool("process poll requires run_id".to_owned())
                })?;
                let snapshot = ctx.async_tool_runs.get(&run_id).await?;
                Ok(ToolExecutionOutcome::completed(
                    snapshot_to_json(&snapshot).to_string(),
                ))
            }
            "cancel" | "kill" => {
                let run_id = args.run_id.ok_or_else(|| {
                    FrameworkError::Tool("process cancel requires run_id".to_owned())
                })?;
                let snapshot = ctx.async_tool_runs.cancel(&run_id).await?;
                Ok(ToolExecutionOutcome::completed(
                    snapshot_to_json(&snapshot).to_string(),
                ))
            }
            "forget" => {
                let run_id = args.run_id.ok_or_else(|| {
                    FrameworkError::Tool("process forget requires run_id".to_owned())
                })?;
                let snapshot = ctx.async_tool_runs.forget(&run_id).await?;
                Ok(ToolExecutionOutcome::completed(
                    snapshot_to_json(&snapshot).to_string(),
                ))
            }
            other => Err(FrameworkError::Tool(format!(
                "process action must be one of list|poll|cancel|kill|forget, got: {other}"
            ))),
        }
    }
}
