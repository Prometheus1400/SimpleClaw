use serde::{Deserialize, Serialize};

use super::defaults::{default_enabled_tools, default_sandbox_enabled};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSandboxConfig {
    pub enabled: bool,
    pub filesystem: AgentSandboxFilesystemConfig,
    pub network: AgentSandboxNetworkConfig,
}

impl Default for AgentSandboxConfig {
    fn default() -> Self {
        Self {
            enabled: default_sandbox_enabled(),
            filesystem: AgentSandboxFilesystemConfig::default(),
            network: AgentSandboxNetworkConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSandboxFilesystemConfig {
    pub extra_writable_paths: Vec<String>,
}

impl Default for AgentSandboxFilesystemConfig {
    fn default() -> Self {
        Self {
            extra_writable_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSandboxNetworkConfig {
    pub mode: SandboxNetworkMode,
}

impl Default for AgentSandboxNetworkConfig {
    fn default() -> Self {
        Self {
            mode: SandboxNetworkMode::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetworkMode {
    Enabled,
    #[default]
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolConfig {
    pub enabled_tools: Vec<String>,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            enabled_tools: default_enabled_tools(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub enabled_skills: Vec<String>,
}
