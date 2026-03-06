use async_trait::async_trait;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

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
        "Manage exec background processes using JSON: {action: list|poll|kill, session_id?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\",\"enum\":[\"list\",\"poll\",\"kill\"]},\"session_id\":{\"type\":\"string\"}},\"required\":[\"action\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args = parse_process_args(args_json);
        match args.action.as_str() {
            "list" => {
                let items = ctx.process_manager.list().await;
                let payload = items
                    .into_iter()
                    .map(|snapshot| snapshot_to_json(&snapshot))
                    .collect::<Vec<_>>();
                Ok(json!({"status":"ok","processes": payload}).to_string())
            }
            "poll" => {
                let session_id = args.session_id.ok_or_else(|| {
                    FrameworkError::Tool("process poll requires session_id".to_owned())
                })?;
                let snapshot = ctx.process_manager.update(&session_id).await?;
                Ok(snapshot_to_json(&snapshot).to_string())
            }
            "kill" => {
                let session_id = args.session_id.ok_or_else(|| {
                    FrameworkError::Tool("process kill requires session_id".to_owned())
                })?;
                let snapshot = ctx.process_manager.kill(&session_id).await?;
                Ok(snapshot_to_json(&snapshot).to_string())
            }
            other => Err(FrameworkError::Tool(format!(
                "process action must be one of list|poll|kill, got: {other}"
            ))),
        }
    }
}
