use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tracing::{Instrument, info_span};

use crate::channels::{Channel, InboundMessage};
use crate::config::{ChannelOutputMode, GatewayChannelKind, RoutingConfig};
use crate::error::FrameworkError;

mod policy;
mod router;
mod session;
mod transport;

pub struct Gateway {
    channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
    output_modes: HashMap<GatewayChannelKind, ChannelOutputMode>,
    inbound_policy: RoutingConfig,
}

pub struct GatewayListeners {
    tasks: Vec<JoinHandle<()>>,
}

impl GatewayListeners {
    pub fn shutdown(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for GatewayListeners {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Gateway {
    pub fn new(
        channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
        output_modes: HashMap<GatewayChannelKind, ChannelOutputMode>,
        inbound_policy: RoutingConfig,
    ) -> Self {
        Self {
            channels,
            output_modes,
            inbound_policy,
        }
    }

    pub fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> GatewayListeners {
        let mut tasks = Vec::with_capacity(self.channels.len());
        for (kind, channel) in &self.channels {
            let kind = *kind;
            let channel = Arc::clone(channel);
            let inbound_tx = inbound_tx.clone();
            let inbound_policy = self.inbound_policy.clone();
            let listener_span = info_span!("gateway.listen");
            tasks.push(tokio::spawn(
                async move {
                    loop {
                        match channel.listen().await {
                            Ok(inbound) => {
                                let Some(inbound) =
                                    router::route_inbound(kind, inbound, &inbound_policy)
                                else {
                                    tracing::debug!(
                                        status = "dropped",
                                        reason = "policy_denied",
                                        "inbound rejected by policy"
                                    );
                                    continue;
                                };
                                tracing::debug!(
                                    status = "routed",
                                    trace_id = %inbound.trace_id,
                                    session_id = %inbound.session_key,
                                    agent_id = %inbound.target_agent_id,
                                    "inbound routed"
                                );
                                if let Err(err) = inbound_tx.send(inbound).await {
                                    tracing::warn!(
                                        status = "dropped",
                                        error_kind = "queue_closed",
                                        error = %err,
                                        "inbound queue closed"
                                    );
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::error!(
                                    status = "retrying",
                                    error_kind = "channel_listen",
                                    error = %err,
                                    "channel listener failed"
                                );
                                sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
                .instrument(listener_span),
            ));
        }

        GatewayListeners { tasks }
    }

    pub async fn send_message(
        &self,
        inbound: &InboundMessage,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel.send_message(&inbound.channel_id, content).await
    }

    pub async fn send_message_with_id(
        &self,
        inbound: &InboundMessage,
        content: &str,
    ) -> Result<Option<String>, FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel
            .send_message_with_id(&inbound.channel_id, content)
            .await
    }

    pub fn supports_message_editing(
        &self,
        inbound: &InboundMessage,
    ) -> Result<bool, FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        Ok(channel.supports_message_editing())
    }

    pub fn output_mode(&self, inbound: &InboundMessage) -> ChannelOutputMode {
        self.output_modes
            .get(&inbound.source_channel)
            .copied()
            .unwrap_or(ChannelOutputMode::Streaming)
    }

    pub async fn edit_message(
        &self,
        inbound: &InboundMessage,
        message_id: &str,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel
            .edit_message(&inbound.channel_id, message_id, content)
            .await
    }

    pub async fn add_reaction(
        &self,
        source_channel: GatewayChannelKind,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, source_channel)?;
        channel.add_reaction(channel_id, message_id, emoji).await
    }

    pub async fn broadcast_typing(&self, inbound: &InboundMessage) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel.broadcast_typing(&inbound.channel_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio::time::Duration;

    use super::Gateway;
    use crate::channels::{Channel, ChannelInbound};
    use crate::config::{ChannelOutputMode, GatewayChannelKind, RoutingConfig};
    use crate::error::FrameworkError;

    struct SingleInboundChannel {
        inbound: Mutex<Option<ChannelInbound>>,
    }

    #[async_trait]
    impl Channel for SingleInboundChannel {
        async fn send_message(
            &self,
            _channel_id: &str,
            _content: &str,
        ) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn add_reaction(
            &self,
            _channel_id: &str,
            _message_id: &str,
            _emoji: &str,
        ) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn broadcast_typing(&self, _channel_id: &str) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
            if let Some(inbound) = self.inbound.lock().await.take() {
                return Ok(inbound);
            }
            pending::<Result<ChannelInbound, FrameworkError>>().await
        }
    }

    #[tokio::test]
    async fn gateway_assigns_source_channel_and_session_key() {
        let inbound = ChannelInbound {
            message_id: "321".to_owned(),
            channel_id: "123".to_owned(),
            guild_id: Some("10".to_owned()),
            is_dm: false,
            user_id: "7".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            content: "hello".to_owned(),
        };
        let channel: Arc<dyn Channel> = Arc::new(SingleInboundChannel {
            inbound: Mutex::new(Some(inbound)),
        });
        let mut channels = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel);
        let (tx, mut rx) = mpsc::channel(1);
        let gateway = Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        );
        let _listeners = gateway.start(tx);
        let next = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("gateway should emit normalized inbound")
            .expect("inbound should decode");
        assert_eq!(next.source_channel, GatewayChannelKind::Discord);
        assert_eq!(next.session_key, "agent:default:discord:123");
        assert_eq!(next.source_message_id.as_deref(), Some("321"));
    }
}
