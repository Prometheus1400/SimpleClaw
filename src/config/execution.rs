use serde::{Deserialize, Serialize};

use super::defaults::{
    default_history_messages, default_max_steps, default_safe_error_reply,
};
use super::tools::{AgentSandboxConfig, SkillsConfig, ToolConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub summon_mode: SummonMode,
    pub owner_ids: Vec<String>,
    pub log_level: LogLevel,
    pub defaults: ExecutionDefaultsConfig,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            summon_mode: SummonMode::default(),
            owner_ids: Vec::new(),
            log_level: LogLevel::default(),
            defaults: ExecutionDefaultsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionDefaultsConfig {
    pub max_steps: u32,
    pub history_messages: u32,
    pub tool_call_transparency: ToolCallTransparency,
    pub memory_preinject: MemoryPreinjectConfig,
    pub safe_error_reply: String,
}

impl Default for ExecutionDefaultsConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            history_messages: default_history_messages(),
            tool_call_transparency: ToolCallTransparency::default(),
            memory_preinject: MemoryPreinjectConfig::default(),
            safe_error_reply: default_safe_error_reply(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallTransparency {
    #[default]
    Off,
    Concise,
    Detailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryPreinjectConfig {
    pub enabled: bool,
    pub top_k: u32,
    pub min_score: f32,
    pub long_term_weight: f32,
    pub max_chars: u32,
}

impl Default for MemoryPreinjectConfig {
    fn default() -> Self {
        use super::defaults::*;
        Self {
            enabled: default_memory_preinject_enabled(),
            top_k: default_memory_preinject_top_k(),
            min_score: default_memory_preinject_min_score(),
            long_term_weight: default_memory_preinject_long_term_weight(),
            max_chars: default_memory_preinject_max_chars(),
        }
    }
}

impl MemoryPreinjectConfig {
    pub fn normalized(&self) -> Self {
        Self {
            enabled: self.enabled,
            top_k: self.top_k.clamp(1, 10),
            min_score: self.min_score.clamp(0.0, 1.0),
            long_term_weight: self.long_term_weight.clamp(0.0, 1.0),
            max_chars: self.max_chars.clamp(200, 4000),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummonMode {
    #[default]
    Synchronous,
    Queued,
}

/// Per-agent config that is flattened into [`super::agents::AgentEntryConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub sandbox: AgentSandboxConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
}
