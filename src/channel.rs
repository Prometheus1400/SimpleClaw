use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::id::ChannelId;
use serenity::prelude::*;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, sleep};

use crate::config::{DiscordConfig, DiscordInboundConfig, GatewayChannelKind};
use crate::error::FrameworkError;

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub source_channel: GatewayChannelKind,
    pub target_agent_id: String,
    pub session_id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
    pub user_id: String,
    pub username: String,
    pub mentioned_bot: bool,
    pub invoke: bool,
    pub content: String,
}

#[async_trait]
pub trait Channel: Send + Sync {
    async fn send_message(&self, session_id: &str, content: &str) -> Result<(), FrameworkError>;
    async fn broadcast_typing(&self, session_id: &str) -> Result<(), FrameworkError>;
    async fn listen(&self) -> Result<InboundMessage, FrameworkError>;
}

pub struct LoggingChannel {
    queue: Mutex<Vec<InboundMessage>>,
    bootstrapped: AtomicBool,
    default_agent_id: String,
}

impl LoggingChannel {
    pub fn new(default_agent_id: String) -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            bootstrapped: AtomicBool::new(false),
            default_agent_id,
        }
    }
}

#[async_trait]
impl Channel for LoggingChannel {
    async fn send_message(&self, session_id: &str, content: &str) -> Result<(), FrameworkError> {
        tracing::info!(session_id, content, "send_message");
        Ok(())
    }

    async fn broadcast_typing(&self, session_id: &str) -> Result<(), FrameworkError> {
        tracing::debug!(session_id, "broadcast_typing");
        Ok(())
    }

    async fn listen(&self) -> Result<InboundMessage, FrameworkError> {
        loop {
            let mut queue = self.queue.lock().await;
            if let Some(msg) = queue.pop() {
                return Ok(msg);
            }
            drop(queue);

            if !self.bootstrapped.swap(true, Ordering::SeqCst) {
                return Ok(InboundMessage {
                    source_channel: GatewayChannelKind::Logging,
                    target_agent_id: self.default_agent_id.clone(),
                    session_id: "bootstrap-session".to_owned(),
                    channel_id: "bootstrap-session".to_owned(),
                    guild_id: None,
                    is_dm: false,
                    user_id: "bootstrap-user".to_owned(),
                    username: "bootstrap-user".to_owned(),
                    mentioned_bot: false,
                    invoke: true,
                    content: "hello agent".to_owned(),
                });
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}

pub struct DiscordChannel {
    http: Arc<Http>,
    inbound_rx: Mutex<mpsc::Receiver<InboundMessage>>,
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
                inbound_policy: config.inbound.clone(),
                bot_user_id: Arc::new(RwLock::new(None)),
            })
            .await
            .map_err(|e| {
                FrameworkError::Config(format!("failed to initialize discord client: {e}"))
            })?;

        let http = Arc::new(Http::new(&token));

        tokio::spawn(async move {
            loop {
                if let Err(err) = client.start_autosharded().await {
                    tracing::error!(error = %err, "discord gateway exited; retrying");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        });

        Ok(Self {
            http,
            inbound_rx: Mutex::new(inbound_rx),
        })
    }
}

struct DiscordHandler {
    inbound_tx: mpsc::Sender<InboundMessage>,
    inbound_policy: DiscordInboundConfig,
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
        let decision = classify_inbound(
            &self.inbound_policy,
            guild_id,
            channel_id,
            is_dm,
            msg.author.id.get(),
            mentioned_bot,
        );
        if !decision.ingest_for_context {
            return;
        }

        let inbound = InboundMessage {
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: decision.target_agent_id,
            session_id: channel_id.to_string(),
            channel_id: channel_id.to_string(),
            guild_id: guild_id.map(|id| id.to_string()),
            is_dm,
            user_id: msg.author.id.get().to_string(),
            username: message_username(&msg),
            mentioned_bot,
            invoke: decision.allow_invoke,
            content: msg.content,
        };

        if let Err(err) = self.inbound_tx.send(inbound).await {
            tracing::warn!(error = %err, "dropping inbound discord message; queue closed");
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    async fn send_message(&self, session_id: &str, content: &str) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(session_id)?;
        channel_id
            .say(&self.http, content)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord send failed: {e}")))?;
        Ok(())
    }

    async fn broadcast_typing(&self, session_id: &str) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(session_id)?;
        channel_id
            .broadcast_typing(&self.http)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord typing failed: {e}")))?;
        Ok(())
    }

    async fn listen(&self) -> Result<InboundMessage, FrameworkError> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundDecision {
    ingest_for_context: bool,
    allow_invoke: bool,
    target_agent_id: String,
}

fn classify_inbound(
    inbound_policy: &DiscordInboundConfig,
    guild_id: Option<u64>,
    channel_id: u64,
    is_dm: bool,
    user_id: u64,
    mentioned_bot: bool,
) -> InboundDecision {
    let policy = inbound_policy.resolve(guild_id, channel_id, is_dm);
    let target_agent_id = policy.agent.trim().to_owned();
    let allow_invoke = !target_agent_id.is_empty()
        && policy.allows_user(user_id)
        && (!policy.require_mentions || mentioned_bot);
    let ingest_for_context = if is_dm { allow_invoke } else { true };
    InboundDecision {
        ingest_for_context,
        allow_invoke,
        target_agent_id,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::classify_inbound;
    use crate::config::{
        DiscordInboundConfig, DiscordInboundPolicyConfig, DiscordServerInboundConfig,
    };

    #[test]
    fn rejects_disallowed_user() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["7".to_owned()]),
                require_mentions: Some(false),
            },
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, Some(10), 20, false, 9, true);
        assert!(decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn rejects_disallowed_dm_user() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                ..DiscordInboundPolicyConfig::default()
            },
            dm: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["7".to_owned()]),
                require_mentions: Some(false),
            },
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, None, 20, true, 9, true);
        assert!(!decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn rejects_when_mentions_required_but_missing() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, Some(10), 20, false, 7, false);
        assert!(decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn accepts_when_mentions_required_and_present() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, Some(10), 20, false, 7, true);
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
    }

    #[test]
    fn dm_ignores_mentions_requirement() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            dm: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["7".to_owned()]),
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, None, 20, true, 7, false);
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
    }

    #[test]
    fn channel_override_takes_precedence() {
        let inbound_policy = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            servers: HashMap::from([(
                "10".to_owned(),
                DiscordServerInboundConfig {
                    policy: DiscordInboundPolicyConfig {
                        agent: Some("reviewer".to_owned()),
                        allow_from: Some(vec!["2".to_owned()]),
                        require_mentions: Some(true),
                    },
                    channels: HashMap::from([(
                        "20".to_owned(),
                        DiscordInboundPolicyConfig {
                            agent: Some("researcher".to_owned()),
                            allow_from: Some(vec!["3".to_owned()]),
                            require_mentions: Some(false),
                        },
                    )]),
                },
            )]),
            ..DiscordInboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, Some(10), 20, false, 3, false);
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
        assert_eq!(decision.target_agent_id, "researcher");
    }
}
