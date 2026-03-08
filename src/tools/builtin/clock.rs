use async_trait::async_trait;
use chrono::Utc;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTool {
    UtcNow,
}

#[async_trait]
impl Tool for ClockTool {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn description(&self) -> &'static str {
        "Current timestamp"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"null\"}"
    }

    async fn execute(
        &self,
        _ctx: &ToolExecEnv,
        _args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        Ok(Utc::now().to_rfc3339())
    }
}
