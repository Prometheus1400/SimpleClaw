use crate::config::GatewayChannelKind;

#[derive(Debug, Clone)]
pub struct ChannelInbound {
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
    pub user_id: String,
    pub username: String,
    pub mentioned_bot: bool,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub trace_id: String,
    pub source_channel: GatewayChannelKind,
    pub target_agent_id: String,
    pub session_key: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
    pub user_id: String,
    pub username: String,
    #[allow(dead_code)]
    pub mentioned_bot: bool,
    pub invoke: bool,
    pub content: String,
}
