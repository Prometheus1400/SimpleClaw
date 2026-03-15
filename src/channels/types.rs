use crate::approval::ApprovalDecision;
use crate::config::GatewayChannelKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InboundMessageKind {
    #[default]
    Text,
    Voice,
}

#[derive(Debug, Clone)]
pub struct ChannelInbound {
    pub message_id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
    pub user_id: String,
    pub username: String,
    pub mentioned_bot: bool,
    pub content: String,
    pub kind: InboundMessageKind,
}

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub trace_id: String,
    pub source_channel: GatewayChannelKind,
    pub target_agent_id: String,
    pub session_key: String,
    pub source_message_id: Option<String>,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
    pub user_id: String,
    pub username: String,
    #[allow(dead_code)]
    pub mentioned_bot: bool,
    pub invoke: bool,
    pub content: String,
    pub kind: InboundMessageKind,
}

#[derive(Debug, Clone)]
pub struct ApprovalResolution {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    pub channel_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone)]
pub struct OutboundVoiceMessage {
    pub audio_bytes: Vec<u8>,
    pub attachment_filename: String,
    pub duration_secs: f64,
    pub waveform: String,
}
