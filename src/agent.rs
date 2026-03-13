use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{AgentInnerConfig, ExecutionDefaultsConfig, TransparencyConfig};
use crate::error::FrameworkError;
use crate::memory::DynMemory;
use crate::prompt::PromptAssembler;
use crate::tools::AgentToolRegistry;

/// Groups declarative parameters needed for an `AgentRuntime`.
#[derive(Debug, Clone)]
pub struct AgentRuntimeConfig {
    pub agent_id: String,
    pub agent_name: String,
    pub provider_key: String,
    pub effective_execution: ExecutionDefaultsConfig,
    pub owner_ids: Vec<String>,
    pub agent_config: AgentInnerConfig,
    pub tool_registry: AgentToolRegistry,
    pub persona_root: PathBuf,
    pub workspace_root: PathBuf,
    #[allow(dead_code)]
    pub app_base_dir: PathBuf,
    pub system_prompt: String,
}

#[derive(Clone)]
pub struct AgentDirectory {
    agent_configs: HashMap<String, AgentRuntimeConfig>,
    memories: HashMap<String, DynMemory>,
}

impl AgentDirectory {
    pub fn new(
        agent_configs: HashMap<String, AgentRuntimeConfig>,
        memories: HashMap<String, DynMemory>,
    ) -> Self {
        Self {
            agent_configs,
            memories,
        }
    }

    pub fn config(&self, agent_id: &str) -> Option<&AgentRuntimeConfig> {
        self.agent_configs.get(agent_id)
    }

    pub fn memory(&self, agent_id: &str) -> Option<&DynMemory> {
        self.memories.get(agent_id)
    }

    pub fn iter_configs(&self) -> impl Iterator<Item = (&String, &AgentRuntimeConfig)> {
        self.agent_configs.iter()
    }
}

pub struct AgentRuntime {
    config: AgentRuntimeConfig,
}

impl AgentRuntime {
    pub fn new(config: AgentRuntimeConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &AgentRuntimeConfig {
        &self.config
    }

    pub fn transparency(&self) -> TransparencyConfig {
        self.config.effective_execution.transparency
    }
}

pub(crate) fn load_system_prompt_for_persona(
    persona_root: &Path,
) -> Result<String, FrameworkError> {
    PromptAssembler::from_persona(persona_root)
}
