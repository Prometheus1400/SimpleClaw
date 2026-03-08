use async_trait::async_trait;

use crate::error::FrameworkError;
use crate::providers::{Message, Role};
use crate::react::RunParams;
use crate::tools::{Tool, ToolExecEnv};

use super::common::parse_task_args;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTool {
    Worker,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Run a delegated worker task using JSON: {prompt}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"prompt\":{\"type\":\"string\"}},\"required\":[\"prompt\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let prompt = parse_task_args(args_json);
        let current = ctx
            .agent_configs
            .get(&ctx.current_agent_id)
            .ok_or_else(|| FrameworkError::Tool("current agent config unavailable".to_owned()))?;
        let mut worker_agent_config = current.agent_config.clone();
        worker_agent_config.tools.enabled_tools = ctx
            .enabled_tools
            .iter()
            .filter(|name| !matches!(name.as_str(), "summon" | "task" | "memorize" | "forget"))
            .cloned()
            .collect();
        let params = RunParams {
            provider_key: &ctx.current_provider_key,
            agent_config: &worker_agent_config,
            system_prompt: "You are a task worker. Complete the assigned task and return a concise result.",
            agent_id: "task-worker",
            session_id,
            max_steps: ctx.max_steps,
            memory: ctx.memory.clone(),
            sandbox: ctx.sandbox.clone(),
            workspace_root: ctx.workspace_root.clone(),
            user_id: ctx.user_id.clone(),
            owner_ids: ctx.owner_ids.clone(),
            process_manager: ctx.process_manager.clone(),
            react_loop: ctx.react_loop.clone(),
            agent_configs: ctx.agent_configs.clone(),
            memories: ctx.memories.clone(),
            enabled_tools: worker_agent_config.tools.enabled_tools.clone(),
            completion_tx: None,
            completion_route: None,
        };
        ctx.react_loop
            .run(params, vec![Message::text(Role::User, prompt)])
            .await
    }
}
