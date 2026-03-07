use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, sleep};

use crate::channel::{Channel, InboundMessage};
use crate::config::GatewayChannelKind;
use crate::error::FrameworkError;

pub struct Gateway {
    channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
    inbound_rx: Mutex<mpsc::Receiver<InboundMessage>>,
}

impl Gateway {
    pub fn new(
        channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
        inbound_tx: mpsc::Sender<InboundMessage>,
        inbound_rx: mpsc::Receiver<InboundMessage>,
    ) -> Self {
        for (kind, channel) in &channels {
            let kind = *kind;
            let channel = Arc::clone(channel);
            let inbound_tx = inbound_tx.clone();
            tokio::spawn(async move {
                loop {
                    match channel.listen().await {
                        Ok(mut inbound) => {
                            inbound.source_channel = kind;
                            if let Err(err) = inbound_tx.send(inbound).await {
                                tracing::warn!(
                                    error = %err,
                                    channel = kind.as_str(),
                                    "dropping inbound message; gateway queue closed"
                                );
                                break;
                            }
                        }
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                channel = kind.as_str(),
                                "channel listener failed; retrying"
                            );
                            sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            });
        }

        Self {
            channels,
            inbound_rx: Mutex::new(inbound_rx),
        }
    }

    pub async fn next_message(&self) -> Result<InboundMessage, FrameworkError> {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| FrameworkError::Config("gateway inbound channel closed".to_owned()))
    }

    pub async fn send_message(
        &self,
        inbound: &InboundMessage,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let channel = self.channels.get(&inbound.source_channel).ok_or_else(|| {
            FrameworkError::Config(format!(
                "missing channel handler for source {}",
                inbound.source_channel.as_str()
            ))
        })?;
        channel.send_message(&inbound.session_id, content).await
    }

    pub async fn broadcast_typing(&self, inbound: &InboundMessage) -> Result<(), FrameworkError> {
        let channel = self.channels.get(&inbound.source_channel).ok_or_else(|| {
            FrameworkError::Config(format!(
                "missing channel handler for source {}",
                inbound.source_channel.as_str()
            ))
        })?;
        channel.broadcast_typing(&inbound.session_id).await
    }
}
