use std::time::Instant;

use tracing::{error, info};

use crate::agent::AgentRuntimeConfig;
use crate::approval::GatewayApprovalRequester;
use crate::channels::InboundMessage;
use crate::error::FrameworkError;
use crate::react::RunParams;
use crate::tools::CompletionRoute;

use super::finalize::finalize_turn;
use super::prepare::{prepare_turn, record_context};
use super::{TurnDisposition, TurnRequest, TurnRuntime};

pub(crate) struct TurnEngine<'a> {
    agent: &'a AgentRuntimeConfig,
    runtime: TurnRuntime<'a>,
}

impl<'a> TurnEngine<'a> {
    pub(crate) fn new(agent: &'a AgentRuntimeConfig, runtime: TurnRuntime<'a>) -> Self {
        Self { agent, runtime }
    }

    #[tracing::instrument(
        name = "agent.turn",
        skip(self, request),
        fields(
            trace_id = %request.inbound.trace_id,
            session_id = %request.memory_session_id,
            agent_id = %self.agent.agent_id,
            persona_root = %self.agent.persona_root.display(),
            workspace_root = %self.agent.workspace_root.display()
        )
    )]
    pub(crate) async fn execute(
        &self,
        request: TurnRequest<'_>,
    ) -> Result<TurnDisposition, FrameworkError> {
        let memory = self.memory()?;
        if !request.inbound.invoke {
            record_context(memory.as_ref(), request.inbound, request.memory_session_id).await?;
            return Ok(TurnDisposition::ContextRecorded);
        }

        let execution_started = Instant::now();
        info!(status = "started", "agent execution");

        let prepared = prepare_turn(
            self.agent,
            memory.as_ref(),
            request.inbound,
            request.memory_session_id,
        )
        .await?;
        let execution_env = self.agent.effective_execution.resolved_env()?;
        let completion_route = completion_route(self.agent, request.inbound);
        let approval_requester = GatewayApprovalRequester::new(
            std::sync::Arc::clone(self.runtime.approval_registry),
            self.runtime.gateway.clone(),
            request.inbound.clone(),
            std::time::Duration::from_secs(300),
        );

        let params = RunParams {
            provider_key: &self.agent.provider_key,
            system_prompt: &prepared.system_prompt,
            agent_id: &self.agent.agent_id,
            agent_name: &self.agent.agent_name,
            session_id: request.memory_session_id,
            max_steps: self.agent.effective_execution.max_steps,
            history_messages: self.agent.effective_execution.history_messages as usize,
            execution_env: &execution_env,
            memory: memory.as_ref(),
            persona_root: &self.agent.persona_root,
            workspace_root: &self.agent.workspace_root,
            user_id: &request.inbound.user_id,
            owner_ids: &self.agent.owner_ids,
            async_tool_runs: self.runtime.async_tool_runs,
            approval_requester: std::sync::Arc::new(approval_requester),
            tool_registry: &self.agent.tool_registry,
            gateway: Some(self.runtime.gateway.as_ref()),
            completion_tx: Some(self.runtime.completion_tx),
            completion_route: Some(&completion_route),
            source_message_id: request.inbound.source_message_id.as_deref(),
            on_text_delta: request.on_text_delta,
            allow_async_tools: true,
        };

        let outcome = match self.runtime.react_loop.run(params, prepared.history).await {
            Ok(outcome) => outcome,
            Err(err) => {
                error!(
                    status = "failed",
                    error_kind = "react_loop",
                    elapsed_ms = execution_started.elapsed().as_millis() as u64,
                    error = %err,
                    "agent execution"
                );
                return Err(err);
            }
        };
        let disposition = finalize_turn(
            memory.as_ref(),
            request.memory_session_id,
            outcome,
            prepared.memory_recall_short_hits,
            prepared.memory_recall_long_hits,
        )
        .await?;

        info!(
            status = "completed",
            elapsed_ms = execution_started.elapsed().as_millis() as u64,
            "agent execution"
        );
        Ok(disposition)
    }

    fn memory(&self) -> Result<&'a crate::memory::DynMemory, FrameworkError> {
        self.runtime
            .directory
            .memory(&self.agent.agent_id)
            .ok_or_else(|| {
                FrameworkError::Config(format!(
                    "missing memory store for agent '{}'",
                    self.agent.agent_id
                ))
            })
    }
}

fn completion_route(agent: &AgentRuntimeConfig, inbound: &InboundMessage) -> CompletionRoute {
    CompletionRoute {
        trace_id: inbound.trace_id.clone(),
        source_channel: inbound.source_channel,
        target_agent_id: agent.agent_id.clone(),
        session_key: inbound.session_key.clone(),
        source_message_id: inbound.source_message_id.clone(),
        channel_id: inbound.channel_id.clone(),
        guild_id: inbound.guild_id.clone(),
        is_dm: inbound.is_dm,
    }
}
