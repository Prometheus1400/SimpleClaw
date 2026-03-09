use std::sync::Arc;

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::channel::MessageReaction;
use serenity::model::channel::ReactionType;
use serenity::model::id::{ChannelId, EmojiId, MessageId};
use serenity::prelude::*;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};
use tracing::{Instrument, info_span};

use crate::channels::{Channel, ChannelInbound};
use crate::config::ChannelConfig;
use crate::error::FrameworkError;

pub struct DiscordChannel {
    http: Arc<Http>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelInbound>>,
}

impl DiscordChannel {
    pub async fn from_config(config: &ChannelConfig) -> Result<Self, FrameworkError> {
        let token = match config.token.clone() {
            Some(token) if !token.trim().is_empty() => token,
            _ => {
                return Err(FrameworkError::Config(
                    "missing Discord token: set gateway.channels.discord.token to a ${secret:<name>} reference"
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
                    tracing::error!(status = "retrying", error_kind = "gateway_exit", error = %err, "discord gateway exited");
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
            message_id: msg.id.get().to_string(),
            channel_id: channel_id.to_string(),
            guild_id: guild_id.map(|id| id.to_string()),
            is_dm,
            user_id: msg.author.id.get().to_string(),
            username: message_username(&msg),
            mentioned_bot,
            content: msg.content,
        };

        tracing::debug!(status = "received", "discord inbound received");
        if let Err(err) = self.inbound_tx.send(inbound).await {
            tracing::warn!(status = "dropped", error_kind = "queue_closed", error = %err, "discord inbound queue closed");
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        tracing::debug!(status = "sending", "discord send");
        channel_id
            .say(&self.http, content)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord send failed: {e}")))?;
        tracing::debug!(status = "completed", "discord send");
        Ok(())
    }

    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let message_id = parse_message_id(message_id)?;
        let reaction = parse_reaction_type(emoji)?;
        let message = channel_id
            .message(&self.http, message_id)
            .await
            .map_err(|e| FrameworkError::Tool(format!("discord fetch message failed: {e}")))?;
        let existing_bot_reactions = collect_bot_reaction_types(&message.reactions);

        channel_id
            .create_reaction(&self.http, message_id, reaction.clone())
            .await
            .map_err(|e| FrameworkError::Tool(format!("discord add reaction failed: {e}")))?;

        for existing in existing_bot_reactions
            .into_iter()
            .filter(|existing| *existing != reaction)
        {
            if let Err(err) = channel_id
                .delete_reaction(&self.http, message_id, None, existing)
                .await
            {
                tracing::warn!(
                    status = "failed",
                    error_kind = "delete_reaction",
                    error = %err,
                    channel_id = %channel_id.get(),
                    message_id = %message_id.get(),
                    "discord reaction cleanup failed"
                );
            }
        }
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

fn parse_message_id(raw: &str) -> Result<MessageId, FrameworkError> {
    let id: u64 = raw
        .parse()
        .map_err(|_| FrameworkError::Tool(format!("invalid discord message id: {raw}")))?;
    Ok(MessageId::new(id))
}

fn parse_reaction_type(raw: &str) -> Result<ReactionType, FrameworkError> {
    let emoji = raw.trim();
    if emoji.is_empty() {
        return Err(FrameworkError::Tool(
            "emoji is required to add a reaction".to_owned(),
        ));
    }

    if let Some(parsed) = parse_custom_reaction_type(emoji)? {
        return Ok(parsed);
    }

    Ok(ReactionType::Unicode(emoji.to_owned()))
}

fn parse_custom_reaction_type(emoji: &str) -> Result<Option<ReactionType>, FrameworkError> {
    let Some(stripped) = emoji
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
    else {
        return Ok(None);
    };

    let (animated, rest) = if let Some(value) = stripped.strip_prefix("a:") {
        (true, value)
    } else if let Some(value) = stripped.strip_prefix(':') {
        (false, value)
    } else {
        return Ok(None);
    };

    let mut parts = rest.split(':');
    let Some(name) = parts.next() else {
        return Err(FrameworkError::Tool(format!(
            "invalid custom emoji format: {emoji}"
        )));
    };
    let Some(id_raw) = parts.next() else {
        return Err(FrameworkError::Tool(format!(
            "invalid custom emoji format: {emoji}"
        )));
    };
    if parts.next().is_some() {
        return Err(FrameworkError::Tool(format!(
            "invalid custom emoji format: {emoji}"
        )));
    }
    let id: u64 = id_raw
        .parse()
        .map_err(|_| FrameworkError::Tool(format!("invalid custom emoji id: {id_raw}")))?;
    Ok(Some(ReactionType::Custom {
        animated,
        id: EmojiId::new(id),
        name: Some(name.to_owned()),
    }))
}

fn collect_bot_reaction_types(reactions: &[MessageReaction]) -> Vec<ReactionType> {
    reactions
        .iter()
        .filter(|reaction| reaction.me)
        .map(|reaction| reaction.reaction_type.clone())
        .collect()
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{collect_bot_reaction_types, parse_reaction_type};
    use serenity::model::channel::MessageReaction;
    use serenity::model::channel::ReactionType;
    use serenity::model::id::EmojiId;

    fn message_reaction(me: bool, reaction_type: ReactionType) -> MessageReaction {
        let emoji = match reaction_type {
            ReactionType::Unicode(name) => json!({ "id": null, "name": name }),
            ReactionType::Custom { animated, id, name } => {
                json!({ "id": id.get().to_string(), "name": name, "animated": animated })
            }
            _ => panic!("unsupported ReactionType variant in test helper"),
        };

        serde_json::from_value(json!({
            "count": 1,
            "count_details": { "burst": 0, "normal": 1 },
            "me": me,
            "me_burst": false,
            "emoji": emoji,
            "burst_colors": []
        }))
        .expect("test reaction should deserialize")
    }

    #[test]
    fn collect_bot_reactions_includes_only_current_user_reactions() {
        let reactions = vec![
            message_reaction(true, ReactionType::Unicode("👀".to_owned())),
            message_reaction(false, ReactionType::Unicode("✅".to_owned())),
        ];

        let selected = collect_bot_reaction_types(&reactions);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0], ReactionType::Unicode("👀".to_owned()));
    }

    #[test]
    fn parse_reaction_type_custom_emoji_keeps_identity() {
        let parsed = parse_reaction_type("<:party:123456>")
            .expect("custom emoji should parse");

        assert_eq!(
            parsed,
            ReactionType::Custom {
                animated: false,
                id: EmojiId::new(123456),
                name: Some("party".to_owned()),
            }
        );
    }
}
