use tokio::sync::oneshot;

use crate::config::GatewayChannelKind;

/// The raw command kind forwarded by a channel before Gateway routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelCommandKind {
    NewSession,
    Status,
    Stop,
    Tools,
}

impl ChannelCommandKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NewSession => "new",
            Self::Status => "status",
            Self::Stop => "stop",
            Self::Tools => "tools",
        }
    }
}

/// A raw command request emitted by a channel.
#[derive(Debug)]
pub struct ChannelCommand {
    pub kind: ChannelCommandKind,
    pub source: GatewayChannelKind,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub user_id: String,
    pub is_dm: bool,
    pub reply_tx: oneshot::Sender<CommandResponse>,
}

/// Gateway's response to a channel command.
#[derive(Debug, Clone)]
pub enum CommandResponse {
    NewSession {
        message: String,
    },
    Status {
        message: String,
    },
    Stop {
        killed_count: usize,
        message: String,
    },
    Tools {
        message: String,
    },
}

impl CommandResponse {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::NewSession { message }
            | Self::Status { message }
            | Self::Stop { message, .. }
            | Self::Tools { message } => message,
        }
    }
}
