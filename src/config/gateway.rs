use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::secrets::Secret;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub channels: HashMap<GatewayChannelKind, ChannelConfig>,
    pub routing: super::routing::RoutingConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let mut channels = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, ChannelConfig::default());
        Self {
            channels,
            routing: super::routing::RoutingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GatewayChannelKind {
    Discord,
}

impl GatewayChannelKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelOutputMode {
    Streaming,
    Normal,
}

impl Default for ChannelOutputMode {
    fn default() -> Self {
        Self::Streaming
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub output: ChannelOutputMode,
    #[serde(default)]
    pub token: Option<Secret<String>>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            output: ChannelOutputMode::Streaming,
            token: None,
        }
    }
}
