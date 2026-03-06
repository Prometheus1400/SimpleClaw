use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{AgentConfig, LoadedConfig};
use crate::error::FrameworkError;
use crate::memory::MemoryStore;
use crate::prompt::PromptAssembler;
use crate::provider::{Message, Provider, Role};
use crate::react;
use crate::tools::{SummonService, ToolCtx};

pub struct AgentRuntime {
    config: LoadedConfig,
    provider: Arc<dyn Provider>,
    memory: MemoryStore,
    system_prompt: String,
    max_steps: u32,
}

impl AgentRuntime {
    pub fn new(
        config: LoadedConfig,
        provider: Arc<dyn Provider>,
        memory: MemoryStore,
        system_prompt: String,
        max_steps: u32,
    ) -> Self {
        Self {
            config,
            provider,
            memory,
            system_prompt,
            max_steps,
        }
    }

    pub async fn run(
        &self,
        inbound: &crate::channel::InboundMessage,
    ) -> Result<String, FrameworkError> {
        self.memory
            .append_message(&inbound.session_id, "user", &inbound.content)
            .await?;
        let history = self.seeded_history(&inbound.session_id).await?;

        let summon_service: Arc<dyn SummonService> = Arc::new(RuntimeSummonService {
            provider: Arc::clone(&self.provider),
            memory: self.memory.clone(),
            summon_agents: self.config.global.runtime.summon_agents.clone(),
            network_allow_all: self.config.global.runtime.network_allow_all,
            read_allow_all: self.config.global.runtime.read_allow_all,
            max_steps: self.max_steps.min(self.config.global.runtime.max_steps),
        });

        let tool_ctx = ToolCtx {
            memory: self.memory.clone(),
            network_allow_all: self.config.global.runtime.network_allow_all,
            read_allow_all: self.config.global.runtime.read_allow_all,
            summon_service: Some(summon_service),
        };

        let reply = react::run_loop(
            self.provider.as_ref(),
            &tool_ctx,
            &self.config.agent.tools,
            &self.system_prompt,
            &inbound.session_id,
            history,
            self.max_steps.min(self.config.global.runtime.max_steps),
        )
        .await?;

        self.memory
            .append_message(&inbound.session_id, "assistant", &reply)
            .await?;
        Ok(reply)
    }

    pub fn config(&self) -> &LoadedConfig {
        &self.config
    }

    pub async fn record_context(
        &self,
        inbound: &crate::channel::InboundMessage,
    ) -> Result<(), FrameworkError> {
        self.memory
            .append_message(&inbound.session_id, "user", &inbound.content)
            .await
    }

    async fn seeded_history(&self, session_id: &str) -> Result<Vec<Message>, FrameworkError> {
        let history_limit = self.config.global.runtime.history_messages as usize;
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
            history.push(Message {
                role,
                content: item.content,
            });
        }
        Ok(history)
    }
}

struct RuntimeSummonService {
    provider: Arc<dyn Provider>,
    memory: MemoryStore,
    summon_agents: std::collections::HashMap<String, PathBuf>,
    network_allow_all: bool,
    read_allow_all: bool,
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

        let system_prompt = PromptAssembler::from_workspace(workspace)?;
        let target_agent_config = load_agent_config(workspace)?;
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
            memory: self.memory.clone(),
            network_allow_all: self.network_allow_all,
            read_allow_all: self.read_allow_all,
            summon_service: None,
        };

        let output = react::run_loop(
            self.provider.as_ref(),
            &tool_ctx,
            &target_agent_config.tools,
            &system_prompt,
            session_id,
            vec![Message {
                role: Role::User,
                content: handoff,
            }],
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
