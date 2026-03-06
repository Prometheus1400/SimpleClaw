use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tracing::{debug, error, info};

use crate::config::{AgentConfig, RuntimeConfig};
use crate::error::FrameworkError;
use crate::memory::MemoryStore;
use crate::prompt::PromptAssembler;
use crate::provider::{Message, Provider, Role};
use crate::react;
use crate::tools::{ProcessManager, SummonService, ToolCtx, ToolRegistry, default_registry};

pub struct AgentRuntime {
    runtime_config: RuntimeConfig,
    agent_config: AgentConfig,
    provider: Arc<dyn Provider>,
    memory: MemoryStore,
    tool_registry: Arc<ToolRegistry>,
    process_manager: Arc<ProcessManager>,
    summon_agents: HashMap<String, PathBuf>,
    summon_memories: HashMap<String, MemoryStore>,
    workspace_root: PathBuf,
    system_prompt: String,
    max_steps: u32,
}

impl AgentRuntime {
    pub fn new(
        runtime_config: RuntimeConfig,
        agent_config: AgentConfig,
        provider: Arc<dyn Provider>,
        memory: MemoryStore,
        summon_agents: HashMap<String, PathBuf>,
        summon_memories: HashMap<String, MemoryStore>,
        workspace_root: PathBuf,
        system_prompt: String,
        max_steps: u32,
    ) -> Self {
        Self {
            runtime_config,
            agent_config,
            provider,
            memory,
            tool_registry: Arc::new(default_registry()),
            process_manager: Arc::new(ProcessManager::new()),
            summon_agents,
            summon_memories,
            workspace_root,
            system_prompt,
            max_steps,
        }
    }

    pub async fn run(
        &self,
        inbound: &crate::channel::InboundMessage,
        memory_session_id: &str,
    ) -> Result<String, FrameworkError> {
        let execution_started = Instant::now();
        info!(
            session_id = %memory_session_id,
            channel_id = %inbound.channel_id,
            user_id = %inbound.user_id,
            max_steps = self.max_steps.min(self.runtime_config.max_steps),
            "agent execution started"
        );
        self.memory
            .append_message(
                memory_session_id,
                "user",
                &inbound.content,
                Some(&inbound.username),
            )
            .await?;
        let history = self.seeded_history(memory_session_id).await?;
        debug!(
            session_id = %memory_session_id,
            history_len = history.len(),
            "loaded seeded history"
        );

        let summon_service: Arc<dyn SummonService> = Arc::new(RuntimeSummonService {
            provider: Arc::clone(&self.provider),
            tool_registry: Arc::clone(&self.tool_registry),
            process_manager: Arc::clone(&self.process_manager),
            summon_agents: self.summon_agents.clone(),
            summon_memories: self.summon_memories.clone(),
            max_steps: self.max_steps.min(self.runtime_config.max_steps),
        });
        let active_tools = self
            .tool_registry
            .resolve_active(&self.agent_config.tools)?;

        let tool_ctx = ToolCtx {
            memory: self.memory.clone(),
            network_allow_all: self.agent_config.network_allow_all,
            read_allow_all: self.agent_config.read_allow_all,
            sandbox: self.agent_config.sandbox,
            workspace_root: self.workspace_root.clone(),
            process_manager: Arc::clone(&self.process_manager),
            summon_service: Some(summon_service),
        };

        let reply = match react::run_loop(
            self.provider.as_ref(),
            &tool_ctx,
            &active_tools,
            &self.system_prompt,
            memory_session_id,
            history,
            self.max_steps.min(self.runtime_config.max_steps),
        )
        .await
        {
            Ok(reply) => reply,
            Err(err) => {
                error!(
                    session_id = %memory_session_id,
                    elapsed_ms = execution_started.elapsed().as_millis() as u64,
                    error = %err,
                    "agent execution failed"
                );
                return Err(err);
            }
        };

        self.memory
            .append_message(memory_session_id, "assistant", &reply, None)
            .await?;
        info!(
            session_id = %memory_session_id,
            elapsed_ms = execution_started.elapsed().as_millis() as u64,
            output_preview = %react::sanitize_log_preview(&reply, 200),
            "agent execution completed"
        );
        Ok(reply)
    }

    pub async fn record_context(
        &self,
        inbound: &crate::channel::InboundMessage,
        memory_session_id: &str,
    ) -> Result<(), FrameworkError> {
        self.memory
            .append_message(
                memory_session_id,
                "user",
                &inbound.content,
                Some(&inbound.username),
            )
            .await
    }

    async fn seeded_history(&self, session_id: &str) -> Result<Vec<Message>, FrameworkError> {
        let history_limit = self.runtime_config.history_messages as usize;
        let stored = self
            .memory
            .recent_messages(session_id, history_limit)
            .await?;
        let mut history = Vec::with_capacity(stored.len());
        for item in stored {
            let role = match item.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => continue,
            };
            let content = if matches!(role, Role::User) {
                if let Some(username) = item.username.as_deref().map(str::trim)
                    && !username.is_empty()
                {
                    format!("[{username}] {}", item.content)
                } else {
                    item.content
                }
            } else {
                item.content
            };
            history.push(Message::text(role, content));
        }
        Ok(history)
    }
}

struct RuntimeSummonService {
    provider: Arc<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    process_manager: Arc<ProcessManager>,
    summon_agents: HashMap<String, PathBuf>,
    summon_memories: HashMap<String, MemoryStore>,
    max_steps: u32,
}

#[async_trait]
impl SummonService for RuntimeSummonService {
    async fn summon(
        &self,
        target_agent: &str,
        summary: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let workspace = self.summon_agents.get(target_agent).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown summon target: {target_agent}"))
        })?;
        let target_memory = self.summon_memories.get(target_agent).ok_or_else(|| {
            FrameworkError::Tool(format!(
                "missing memory store for summon target: {target_agent}"
            ))
        })?;

        let system_prompt = PromptAssembler::from_workspace(workspace)?;
        let target_agent_config = load_agent_config(workspace)?;
        let active_tools = self
            .tool_registry
            .resolve_active(&target_agent_config.tools)?;
        let handoff = if summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{target_agent}`. Continue from session context and produce a final answer."
            )
        } else {
            format!(
                "You were summoned as agent `{target_agent}` with handoff summary:\n{}",
                summary
            )
        };

        let tool_ctx = ToolCtx {
            memory: target_memory.clone(),
            network_allow_all: target_agent_config.network_allow_all,
            read_allow_all: target_agent_config.read_allow_all,
            sandbox: target_agent_config.sandbox,
            workspace_root: workspace.clone(),
            process_manager: Arc::clone(&self.process_manager),
            summon_service: None,
        };

        let output = react::run_loop(
            self.provider.as_ref(),
            &tool_ctx,
            &active_tools,
            &system_prompt,
            session_id,
            vec![Message::text(Role::User, handoff)],
            self.max_steps,
        )
        .await?;

        Ok(output)
    }
}

fn load_agent_config(workspace: &Path) -> Result<AgentConfig, FrameworkError> {
    let agent_path = workspace.join("agent.yaml");
    if !agent_path.exists() {
        return Ok(AgentConfig::default());
    }

    let content = fs::read_to_string(agent_path)?;
    Ok(serde_yaml::from_str::<AgentConfig>(&content)?)
}

pub(crate) fn load_agent_config_for_workspace(
    workspace: &Path,
) -> Result<AgentConfig, FrameworkError> {
    load_agent_config(workspace)
}

pub(crate) fn load_system_prompt_for_workspace(workspace: &Path) -> Result<String, FrameworkError> {
    PromptAssembler::from_workspace(workspace)
}
