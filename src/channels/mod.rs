mod discord;
pub(crate) mod policy;
mod types;

use async_trait::async_trait;

use crate::error::FrameworkError;

pub use discord::DiscordChannel;
pub use types::{ChannelInbound, InboundMessage};

#[async_trait]
pub trait Channel: Send + Sync {
    fn supports_message_editing(&self) -> bool {
        false
    }

    fn message_char_limit(&self) -> Option<usize> {
        None
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError>;
    async fn send_message_with_id(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Option<String>, FrameworkError> {
        self.send_message(channel_id, content).await?;
        Ok(None)
    }
    async fn edit_message(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _content: &str,
    ) -> Result<(), FrameworkError> {
        Err(FrameworkError::Tool(
            "channel does not support editing messages".to_owned(),
        ))
    }
    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), FrameworkError>;
    async fn broadcast_typing(&self, channel_id: &str) -> Result<(), FrameworkError>;
    async fn listen(&self) -> Result<ChannelInbound, FrameworkError>;
}
