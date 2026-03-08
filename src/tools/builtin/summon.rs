use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::providers::{Message, Role};
use crate::react::RunParams;
use crate::tools::{Tool, ToolExecEnv};

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
        let target_config = ctx
            .agent_configs
            .get(&target)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown agent: {target}")))?;
        let memory = ctx
            .memories
            .get(&target)
            .ok_or_else(|| FrameworkError::Tool(format!("no memory for agent: {target}")))?;
        let handoff = if summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{target}`. Continue from session context and produce a final answer."
            )
        } else {
            format!("You were summoned as agent `{target}` with handoff summary:\n{summary}")
        };

        let effective_max_steps = target_config
            .max_steps
            .min(target_config.runtime_config.max_steps);
        let params = RunParams {
            provider_key: &target_config.provider_key,
            agent_config: &target_config.agent_config,
            system_prompt: &target_config.system_prompt,
            agent_id: &target,
            session_id,
            max_steps: effective_max_steps,
            memory: memory.clone(),
            sandbox: target_config.agent_config.sandbox.clone(),
            workspace_root: target_config.workspace_root.clone(),
            user_id: ctx.user_id.clone(),
            owner_ids: target_config.runtime_config.owner_ids.clone(),
            process_manager: ctx.process_manager.clone(),
            react_loop: ctx.react_loop.clone(),
            agent_configs: ctx.agent_configs.clone(),
            memories: ctx.memories.clone(),
            enabled_tools: target_config.agent_config.tools.enabled_tools.clone(),
            completion_tx: None,
            completion_route: None,
        };
        ctx.react_loop
            .run(params, vec![Message::text(Role::User, handoff)])
            .await
    }
}
