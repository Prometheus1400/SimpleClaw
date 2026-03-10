use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::defaults::{default_history_messages, default_max_steps, default_safe_error_reply};

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
    pub env: BTreeMap<String, String>,
    pub transparency: TransparencyConfig,
    pub memory_recall: MemoryRecallConfig,
    pub safe_error_reply: String,
}

impl Default for ExecutionDefaultsConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            history_messages: default_history_messages(),
            env: BTreeMap::new(),
            transparency: TransparencyConfig::default(),
            memory_recall: MemoryRecallConfig::default(),
            safe_error_reply: default_safe_error_reply(),
        }
    }
}

impl ExecutionDefaultsConfig {
    pub fn merge_with_overrides(&self, overrides: &AgentExecutionOverrides) -> Self {
        let memory_recall = match &overrides.memory_recall {
            Some(memory_overrides) => MemoryRecallConfig {
                enabled: memory_overrides
                    .enabled
                    .unwrap_or(self.memory_recall.enabled),
                top_k: memory_overrides.top_k.unwrap_or(self.memory_recall.top_k),
                min_score: memory_overrides
                    .min_score
                    .unwrap_or(self.memory_recall.min_score),
                long_term_weight: memory_overrides
                    .long_term_weight
                    .unwrap_or(self.memory_recall.long_term_weight),
                max_chars: memory_overrides
                    .max_chars
                    .unwrap_or(self.memory_recall.max_chars),
            },
            None => self.memory_recall.clone(),
        };

        Self {
            max_steps: overrides.max_steps.unwrap_or(self.max_steps),
            history_messages: overrides.history_messages.unwrap_or(self.history_messages),
            env: {
                let mut env = self.env.clone();
                if let Some(overrides_env) = &overrides.env {
                    env.extend(overrides_env.clone());
                }
                env
            },
            transparency: TransparencyConfig {
                tool_calls: overrides
                    .transparency
                    .as_ref()
                    .and_then(|value| value.tool_calls)
                    .unwrap_or(self.transparency.tool_calls),
                memory_recall: overrides
                    .transparency
                    .as_ref()
                    .and_then(|value| value.memory_recall)
                    .unwrap_or(self.transparency.memory_recall),
            },
            memory_recall,
            safe_error_reply: overrides
                .safe_error_reply
                .clone()
                .unwrap_or_else(|| self.safe_error_reply.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TransparencyConfig {
    pub tool_calls: bool,
    pub memory_recall: bool,
}

impl Default for TransparencyConfig {
    fn default() -> Self {
        Self {
            tool_calls: false,
            memory_recall: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryRecallConfig {
    pub enabled: bool,
    pub top_k: u32,
    pub min_score: f32,
    pub long_term_weight: f32,
    pub max_chars: u32,
}

impl Default for MemoryRecallConfig {
    fn default() -> Self {
        use super::defaults::*;
        Self {
            enabled: default_memory_recall_enabled(),
            top_k: default_memory_recall_top_k(),
            min_score: default_memory_recall_min_score(),
            long_term_weight: default_memory_recall_long_term_weight(),
            max_chars: default_memory_recall_max_chars(),
        }
    }
}

impl MemoryRecallConfig {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentExecutionOverrides {
    pub max_steps: Option<u32>,
    pub history_messages: Option<u32>,
    pub env: Option<BTreeMap<String, String>>,
    pub transparency: Option<TransparencyOverrides>,
    pub memory_recall: Option<MemoryRecallOverrides>,
    pub safe_error_reply: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TransparencyOverrides {
    pub tool_calls: Option<bool>,
    pub memory_recall: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryRecallOverrides {
    pub enabled: Option<bool>,
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub long_term_weight: Option<f32>,
    pub max_chars: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentExecutionOverrides, ExecutionDefaultsConfig, MemoryRecallOverrides,
        TransparencyOverrides,
    };
    use std::collections::BTreeMap;

    #[test]
    fn merge_with_overrides_applies_only_provided_fields() {
        let defaults = ExecutionDefaultsConfig::default();
        let overrides = AgentExecutionOverrides {
            max_steps: Some(42),
            history_messages: None,
            env: Some(BTreeMap::from([("CHILD".to_owned(), "override".to_owned())])),
            transparency: Some(TransparencyOverrides {
                tool_calls: Some(true),
                memory_recall: Some(true),
            }),
            memory_recall: Some(MemoryRecallOverrides {
                enabled: Some(false),
                top_k: None,
                min_score: Some(0.9),
                long_term_weight: None,
                max_chars: Some(2500),
            }),
            safe_error_reply: Some("custom".to_owned()),
        };
        let defaults = ExecutionDefaultsConfig {
            env: BTreeMap::from([
                ("BASE".to_owned(), "base".to_owned()),
                ("CHILD".to_owned(), "default".to_owned()),
            ]),
            ..defaults
        };

        let merged = defaults.merge_with_overrides(&overrides);
        assert_eq!(merged.max_steps, 42);
        assert_eq!(merged.history_messages, defaults.history_messages);
        assert_eq!(
            merged.env,
            BTreeMap::from([
                ("BASE".to_owned(), "base".to_owned()),
                ("CHILD".to_owned(), "override".to_owned()),
            ])
        );
        assert!(merged.transparency.tool_calls);
        assert!(merged.transparency.memory_recall);
        assert!(!merged.memory_recall.enabled);
        assert_eq!(merged.memory_recall.top_k, defaults.memory_recall.top_k);
        assert!((merged.memory_recall.min_score - 0.9).abs() < f32::EPSILON);
        assert_eq!(
            merged.memory_recall.long_term_weight,
            defaults.memory_recall.long_term_weight
        );
        assert_eq!(merged.memory_recall.max_chars, 2500);
        assert_eq!(merged.safe_error_reply, "custom");
    }

    #[test]
    fn merge_with_overrides_returns_defaults_when_empty() {
        let defaults = ExecutionDefaultsConfig::default();
        let merged = defaults.merge_with_overrides(&AgentExecutionOverrides::default());
        assert_eq!(merged.max_steps, defaults.max_steps);
        assert_eq!(merged.history_messages, defaults.history_messages);
        assert_eq!(merged.env, defaults.env);
        assert_eq!(
            merged.transparency.tool_calls,
            defaults.transparency.tool_calls
        );
        assert_eq!(
            merged.transparency.memory_recall,
            defaults.transparency.memory_recall
        );
        assert_eq!(
            merged.memory_recall.long_term_weight,
            defaults.memory_recall.long_term_weight
        );
        assert_eq!(merged.safe_error_reply, defaults.safe_error_reply);
    }
}
