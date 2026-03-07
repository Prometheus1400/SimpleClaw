use std::sync::Arc;

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::id::ChannelId;
use serenity::prelude::*;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};
use tracing::{Instrument, info_span};

use crate::channels::{Channel, ChannelInbound};
use crate::config::DiscordConfig;
use crate::error::FrameworkError;

pub struct DiscordChannel {
    http: Arc<Http>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelInbound>>,
}

impl DiscordChannel {
    pub async fn from_config(config: &DiscordConfig) -> Result<Self, FrameworkError> {
        let token = match config.token.clone() {
            Some(token) if !token.trim().is_empty() => token,
            _ => {
                return Err(FrameworkError::Config(
                    "missing Discord token: set discord.token to a ${secret:<name>} reference"
                        .to_owned(),
                ));
            }
        };

        let (inbound_tx, inbound_rx) = mpsc::channel(1_024);
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let mut client = Client::builder(token.clone(), intents)
            .event_handler(DiscordHandler {
                inbound_tx,
                bot_user_id: Arc::new(RwLock::new(None)),
            })
            .await
            .map_err(|e| {
                FrameworkError::Config(format!("failed to initialize discord client: {e}"))
            })?;

        let http = Arc::new(Http::new(&token));

        let discord_span = info_span!("channel.discord");
        tokio::spawn(async move {
            loop {
                if let Err(err) = client.start_autosharded().await {
                    tracing::error!(status = "retrying", error_kind = "gateway_exit", error = %err, backoff_ms = 5_000u64, "discord gateway exited");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
        .instrument(discord_span));

        Ok(Self {
            http,
            inbound_rx: Mutex::new(inbound_rx),
        })
    }
}

struct DiscordHandler {
    inbound_tx: mpsc::Sender<ChannelInbound>,
    bot_user_id: Arc<RwLock<Option<u64>>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: serenity::model::gateway::Ready) {
        let mut bot_user_id = self.bot_user_id.write().await;
        *bot_user_id = Some(ready.user.id.get());
    }

    async fn message(&self, _ctx: Context, msg: DiscordMessage) {
        if msg.author.bot || msg.content.trim().is_empty() {
            return;
        }

        let guild_id = msg.guild_id.map(|id| id.get());
        let channel_id = msg.channel_id.get();
        let is_dm = guild_id.is_none();
        let bot_user_id = *self.bot_user_id.read().await;
        let mentioned_bot = bot_user_id
            .map(|id| message_mentions_user(&msg, id))
            .unwrap_or(false);

        let inbound = ChannelInbound {
            channel_id: channel_id.to_string(),
            guild_id: guild_id.map(|id| id.to_string()),
            is_dm,
            user_id: msg.author.id.get().to_string(),
            username: message_username(&msg),
            mentioned_bot,
            content: msg.content,
        };

        tracing::debug!(
            status = "received",
            channel_id = %inbound.channel_id,
            user_id = %inbound.user_id,
            is_dm = inbound.is_dm,
            mentioned_bot = inbound.mentioned_bot,
            content_preview = %crate::telemetry::sanitize_preview(&inbound.content, 96),
            "discord inbound received"
        );
        if let Err(err) = self.inbound_tx.send(inbound).await {
            tracing::warn!(status = "dropped", error_kind = "queue_closed", error = %err, "discord inbound queue closed");
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        tracing::debug!(
            status = "sending",
            channel_id = %channel_id.get(),
            content_preview = %crate::telemetry::sanitize_preview(content, 96),
            "discord send"
        );
        channel_id
            .say(&self.http, content)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord send failed: {e}")))?;
        tracing::debug!(status = "completed", channel_id = %channel_id.get(), "discord send");
        Ok(())
    }

    async fn broadcast_typing(&self, channel_id: &str) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        channel_id
            .broadcast_typing(&self.http)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord typing failed: {e}")))?;
        Ok(())
    }

    async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| FrameworkError::Config("discord inbound channel closed".to_owned()))
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId, FrameworkError> {
    let id: u64 = raw
        .parse()
        .map_err(|_| FrameworkError::Tool(format!("invalid discord channel id: {raw}")))?;
    Ok(ChannelId::new(id))
}

fn message_mentions_user(msg: &DiscordMessage, user_id: u64) -> bool {
    if msg.mentions.iter().any(|user| user.id.get() == user_id) {
        return true;
    }

    let mention_plain = format!("<@{user_id}>");
    let mention_nick = format!("<@!{user_id}>");
    msg.content.contains(&mention_plain) || msg.content.contains(&mention_nick)
}

fn message_username(msg: &DiscordMessage) -> String {
    if let Some(member) = &msg.member
        && let Some(nick) = member.nick.as_deref()
    {
        let trimmed = nick.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Some(global_name) = msg.author.global_name.as_deref() {
        let trimmed = global_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    let trimmed = msg.author.name.trim();
    if trimmed.is_empty() {
        msg.author.id.get().to_string()
    } else {
        trimmed.to_owned()
    }
}
