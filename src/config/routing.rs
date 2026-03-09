use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::defaults::default_agent_id;
use super::gateway::GatewayChannelKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboundConfig {
    pub defaults: InboundPolicyConfig,
    pub channels: HashMap<GatewayChannelKind, ChannelInboundConfig>,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            defaults: InboundPolicyConfig {
                agent: Some(default_agent_id()),
                ..InboundPolicyConfig::default()
            },
            channels: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelInboundConfig {
    #[serde(flatten)]
    pub policy: InboundPolicyConfig,
    #[serde(default)]
    pub dm: InboundPolicyConfig,
    #[serde(default)]
    pub workspaces: HashMap<String, WorkspaceInboundConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceInboundConfig {
    #[serde(flatten)]
    pub policy: InboundPolicyConfig,
    #[serde(default)]
    pub channels: HashMap<String, InboundPolicyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct InboundPolicyConfig {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,
    #[serde(default)]
    pub require_mentions: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct InboundPolicy {
    pub agent: String,
    pub allow_from: Option<Vec<String>>,
    pub require_mentions: bool,
}

impl InboundConfig {
    pub fn resolve(
        &self,
        source_channel: GatewayChannelKind,
        workspace_id: Option<&str>,
        channel_id: &str,
        is_dm: bool,
    ) -> InboundPolicy {
        let mut effective = self.defaults.clone();

        if let Some(channel_scope) = self.channels.get(&source_channel) {
            effective.apply_override(&channel_scope.policy);
            if is_dm {
                effective.apply_override(&channel_scope.dm);
                return effective.finalize(true);
            }
            if !is_dm
                && let Some(workspace) =
                    workspace_id.and_then(|id| channel_scope.workspaces.get(id))
            {
                effective.apply_override(&workspace.policy);
                if let Some(channel) = workspace.channels.get(channel_id) {
                    effective.apply_override(channel);
                }
            }
        }

        if is_dm {
            return effective.finalize(true);
        }

        effective.finalize(false)
    }
}

impl InboundPolicyConfig {
    fn apply_override(&mut self, lower: &InboundPolicyConfig) {
        if let Some(agent) = lower.agent.as_deref() {
            self.agent = Some(agent.to_owned());
        }
        if let Some(allow_from) = &lower.allow_from {
            self.allow_from = Some(allow_from.clone());
        }
        if let Some(require_mentions) = lower.require_mentions {
            self.require_mentions = Some(require_mentions);
        }
    }

    fn finalize(self, is_dm: bool) -> InboundPolicy {
        InboundPolicy {
            agent: self.agent.unwrap_or_else(default_agent_id),
            allow_from: self.allow_from,
            require_mentions: if is_dm {
                false
            } else {
                self.require_mentions.unwrap_or(false)
            },
        }
    }
}

impl InboundPolicy {
    pub fn allows_user(&self, user_id: &str) -> bool {
        match &self.allow_from {
            Some(ids) => ids.iter().any(|id| id == user_id),
            None => true,
        }
    }
}
