use async_trait::async_trait;

use crate::config::SummonToolConfig;
use crate::error::FrameworkError;
use crate::tools::{AgentInvokeRequest, Tool, ToolExecEnv, ToolExecutionOutcome, ToolRunOutput};

use super::common::parse_summon_args;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummonTool {
    config: SummonToolConfig,
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

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.summon config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        self.execute_with_trace(ctx, args_json, session_id).await
    }

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let (target, summary) = parse_summon_args(args_json);
        if !self.target_allowed(&target) {
            return Err(FrameworkError::Tool(format!(
                "summon target '{target}' is not allowed by tools.summon.allowed"
            )));
        }
        let handoff = if summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{target}`. Continue from session context and produce a final answer."
            )
        } else {
            format!("You were summoned as agent `{target}` with handoff summary:\n{summary}")
        };
        let outcome = ctx
            .invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: target,
                session_id: session_id.to_owned(),
                user_id: ctx.user_id.clone(),
                prompt: handoff,
            })
            .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput {
            output: outcome.reply,
            nested_tool_calls: outcome.tool_calls,
        }))
    }
}

impl SummonTool {
    fn target_allowed(&self, target: &str) -> bool {
        self.config.allowed.iter().any(|allowed| allowed == target)
    }
}

#[cfg(test)]
mod tests {
    use super::SummonTool;

    #[test]
    fn empty_allowlist_denies_any_target() {
        let tool = SummonTool::default();
        assert!(!tool.target_allowed("planner"));
        assert!(!tool.target_allowed("reviewer"));
    }

    #[test]
    fn non_empty_allowlist_restricts_target() {
        let mut tool = SummonTool::default();
        tool.config.allowed = vec!["planner".to_owned(), "researcher".to_owned()];
        assert!(tool.target_allowed("planner"));
        assert!(!tool.target_allowed("reviewer"));
    }
}
