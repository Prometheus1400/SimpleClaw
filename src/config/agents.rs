use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::defaults::{default_agent_id, default_agents_list};
use super::execution::AgentExecutionOverrides;
use super::tools::ToolsConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default = "default_agent_id")]
    pub default: String,
    #[serde(default = "default_agents_list")]
    pub list: Vec<AgentEntryConfig>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default: default_agent_id(),
            list: default_agents_list(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentEntryConfig {
    pub id: String,
    pub name: String,
    pub persona: PathBuf,
    pub workspace: PathBuf,
    #[serde(default)]
    pub config: AgentInnerConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentInnerConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub execution: AgentExecutionOverrides,
    #[serde(default)]
    pub tools: ToolsConfig,
}
