use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::tools::{AgentInvokeRequest, Tool, ToolExecEnv};

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
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (target, summary) = parse_summon_args(args_json);
        let handoff = if summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{target}`. Continue from session context and produce a final answer."
            )
        } else {
            format!("You were summoned as agent `{target}` with handoff summary:\n{summary}")
        };
        ctx.invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: target,
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt: handoff,
            })
            .await
    }
}
