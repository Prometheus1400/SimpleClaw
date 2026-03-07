mod discord;
pub(crate) mod policy;
mod types;

use async_trait::async_trait;

use crate::error::FrameworkError;

pub use discord::DiscordChannel;
pub use types::{ChannelInbound, InboundMessage};

#[async_trait]
pub trait Channel: Send + Sync {
    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError>;
    async fn broadcast_typing(&self, channel_id: &str) -> Result<(), FrameworkError>;
    async fn listen(&self) -> Result<ChannelInbound, FrameworkError>;
}
