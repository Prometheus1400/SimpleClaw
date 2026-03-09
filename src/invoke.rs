use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::AgentDirectory;
use crate::error::FrameworkError;
use crate::providers::{Message, Role};
use crate::react::{ReactLoop, RunParams};
use crate::tools::{
    AgentInvokeRequest, AgentInvoker, InvokeOutcome, ProcessManager, WorkerInvokeRequest,
};

/// Implements agent-to-agent invocation by looking up configs in the
/// [`AgentDirectory`] and recursing through the [`ReactLoop`].
///
/// Constructed once at composition time and injected into `ReactLoop`
/// via [`ReactLoop::set_invoker`].
pub(crate) struct DirectAgentInvoker {
    react_loop: Arc<ReactLoop>,
    agents: Arc<AgentDirectory>,
    process_manager: Arc<ProcessManager>,
}

impl DirectAgentInvoker {
    pub fn new(
        react_loop: Arc<ReactLoop>,
        agents: Arc<AgentDirectory>,
        process_manager: Arc<ProcessManager>,
    ) -> Self {
        Self {
            react_loop,
            agents,
            process_manager,
        }
    }
}

#[async_trait]
impl AgentInvoker for DirectAgentInvoker {
    async fn invoke_agent(
        &self,
        request: AgentInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError> {
        let target_config = self
            .agents
            .config(&request.target_agent_id)
            .ok_or_else(|| {
                FrameworkError::Tool(format!("unknown agent: {}", request.target_agent_id))
            })?;
        let memory = self
            .agents
            .memory(&request.target_agent_id)
            .cloned()
            .ok_or_else(|| {
                FrameworkError::Tool(format!("no memory for agent: {}", request.target_agent_id))
            })?;
        let effective_max_steps = target_config.effective_execution.max_steps;
        let params = RunParams {
            provider_key: &target_config.provider_key,
            agent_config: &target_config.agent_config,
            system_prompt: &target_config.system_prompt,
            agent_id: &request.target_agent_id,
            session_id: &request.session_id,
            max_steps: effective_max_steps,
            memory,
            workspace_root: target_config.workspace_root.clone(),
            user_id: request.user_id,
            owner_ids: target_config.owner_ids.clone(),
            process_manager: Arc::clone(&self.process_manager),
            completion_tx: None,
            completion_route: None,
        };
        self.react_loop
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
            .map(|outcome| InvokeOutcome {
                reply: outcome.reply,
                tool_calls: outcome.tool_calls,
            })
    }

    async fn invoke_worker(
        &self,
        request: WorkerInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError> {
        let current_config = self
            .agents
            .config(&request.current_agent_id)
            .ok_or_else(|| FrameworkError::Tool("current agent config unavailable".to_owned()))?;
        let memory = self
            .agents
            .memory(&request.current_agent_id)
            .cloned()
            .ok_or_else(|| FrameworkError::Tool("current agent memory unavailable".to_owned()))?;
        let mut worker_agent_config = current_config.agent_config.clone();
        worker_agent_config.tools =
            worker_agent_config
                .tools
                .with_disabled(&["summon", "task", "memorize", "forget"]);
        let params = RunParams {
            provider_key: &current_config.provider_key,
            agent_config: &worker_agent_config,
            system_prompt: "You are a task worker. Complete the assigned task and return a concise result.",
            agent_id: "task-worker",
            session_id: &request.session_id,
            max_steps: current_config.effective_execution.max_steps,
            memory,
            workspace_root: current_config.workspace_root.clone(),
            user_id: request.user_id,
            owner_ids: current_config.owner_ids.clone(),
            process_manager: Arc::clone(&self.process_manager),
            completion_tx: None,
            completion_route: None,
        };
        self.react_loop
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
            .map(|outcome| InvokeOutcome {
                reply: outcome.reply,
                tool_calls: outcome.tool_calls,
            })
    }
}
