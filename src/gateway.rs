use std::sync::Arc;

use crate::channel::{Channel, InboundMessage};
use crate::error::FrameworkError;

pub struct Gateway {
    channel: Arc<dyn Channel>,
}

impl Gateway {
    pub fn new(channel: Arc<dyn Channel>) -> Self {
        Self { channel }
    }

    pub async fn next_message(&self) -> Result<InboundMessage, FrameworkError> {
        self.channel.listen().await
    }
}
