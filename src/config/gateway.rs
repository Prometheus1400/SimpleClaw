use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token: None,
        }
    }
}
