use serde::{Deserialize, Serialize};

use super::defaults::default_gateway_channels;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub channels: Vec<GatewayChannelKind>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            channels: default_gateway_channels(),
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    #[serde(default)]
    pub token: Option<String>,
}
