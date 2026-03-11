use async_trait::async_trait;

use crate::config::ReactToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv, ToolExecutionOutcome};

use super::common::parse_react_args;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReactTool {
    config: ReactToolConfig,
}

#[async_trait]
impl Tool for ReactTool {
    fn name(&self) -> &'static str {
        "react"
    }

    fn description(&self) -> &'static str {
        "Add an emoji reaction to the current inbound message using JSON: {emoji}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"emoji\":{\"type\":\"string\"}},\"required\":[\"emoji\"]}"
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.react config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let emoji = parse_react_args(args_json);
        if emoji.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "react requires a non-empty emoji".to_owned(),
            ));
        }
        let gateway = ctx.gateway.as_ref().ok_or_else(|| {
            FrameworkError::Tool("react unavailable: gateway context is missing".to_owned())
        })?;
        let route = ctx.completion_route.as_ref().ok_or_else(|| {
            FrameworkError::Tool("react unavailable: completion route is missing".to_owned())
        })?;
        let message_id = route.source_message_id.as_deref().ok_or_else(|| {
            FrameworkError::Tool(
                "react unavailable: current inbound message id is unavailable".to_owned(),
            )
        })?;

        gateway
            .add_reaction(
                route.source_channel,
                route.channel_id.as_str(),
                message_id,
                emoji.trim(),
            )
            .await?;
        Ok(ToolExecutionOutcome::completed("ok".to_owned()))
    }
}
