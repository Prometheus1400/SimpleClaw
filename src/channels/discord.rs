use std::sync::Arc;

use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::json;
use serenity::all::{
    ButtonStyle, Command, CommandInteraction, ComponentInteraction, CreateActionRow,
    CreateAttachment, CreateButton, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, Interaction,
};
use serenity::builder::EditMessage;
use serenity::http::Http;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::channel::MessageReaction;
use serenity::model::channel::ReactionType;
use serenity::model::id::{ChannelId, EmojiId, MessageId};
use serenity::prelude::*;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::time::{Duration, sleep, timeout};
use tracing::{Instrument, info_span};

use crate::approval::{ApprovalDecision, ApprovalRegistry, PendingApprovalRequest};
use crate::audio::Transcriber;
use crate::channels::{
    ApprovalResolution, Channel, ChannelInbound, InboundMessageKind, OutboundVoiceMessage,
};
use crate::config::ChannelConfig;
use crate::error::FrameworkError;
use crate::gateway::{ChannelCommand, ChannelCommandKind, CommandResponse};

const VOICE_TRANSCRIPTION_UNAVAILABLE_PLACEHOLDER: &str = "[Voice message received, but transcription is unavailable. Reply in text and ask the user to retry later if needed.]";

pub struct DiscordChannel {
    api_client: reqwest::Client,
    http: Arc<Http>,
    token: String,
    inbound_rx: Mutex<mpsc::Receiver<ChannelInbound>>,
    approval_rx: Mutex<mpsc::Receiver<ApprovalResolution>>,
    command_rx: Mutex<mpsc::Receiver<ChannelCommand>>,
}

impl DiscordChannel {
    pub async fn from_config(
        config: &ChannelConfig,
        approval_registry: Arc<ApprovalRegistry>,
        transcriber: Option<Arc<dyn Transcriber>>,
    ) -> Result<Self, FrameworkError> {
        let token = match config.token.clone() {
            Some(token)
                if token
                    .exposed()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_some() =>
            {
                token.exposed().expect("checked above").to_owned()
            }
            _ => {
                return Err(FrameworkError::Config(
                    "missing Discord token: set gateway.channels.discord.token to a ${secret:<name>} reference"
                        .to_owned(),
                ));
            }
        };

        let (inbound_tx, inbound_rx) = mpsc::channel(1_024);
        let (approval_tx, approval_rx) = mpsc::channel(1_024);
        let (command_tx, command_rx) = mpsc::channel(1_024);
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILD_MESSAGE_REACTIONS;

        let mut client = Client::builder(token.clone(), intents)
            .event_handler(DiscordHandler {
                inbound_tx,
                approval_tx,
                command_tx,
                approval_registry,
                bot_user_id: Arc::new(RwLock::new(None)),
                http_client: reqwest::Client::new(),
                transcriber,
            })
            .await
            .map_err(|e| {
                FrameworkError::Config(format!("failed to initialize discord client: {e}"))
            })?;

        let http = Arc::new(Http::new(&token));

        let discord_span = info_span!("channel.discord");
        tokio::spawn(async move {
            tracing::info!(status = "connecting", "discord gateway connecting");
            loop {
                match client.start_autosharded().await {
                    Ok(()) => {
                        tracing::warn!(
                            status = "disconnected",
                            error_kind = "gateway_clean_exit",
                            "discord gateway closed cleanly; reconnecting"
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                            status = "retrying",
                            error_kind = "gateway_exit",
                            error = %err,
                            "discord gateway exited"
                        );
                    }
                }
                sleep(Duration::from_secs(5)).await;
            }
        }
        .instrument(discord_span));

        Ok(Self {
            api_client: reqwest::Client::new(),
            http,
            token,
            inbound_rx: Mutex::new(inbound_rx),
            approval_rx: Mutex::new(approval_rx),
            command_rx: Mutex::new(command_rx),
        })
    }
}

struct DiscordHandler {
    inbound_tx: mpsc::Sender<ChannelInbound>,
    approval_tx: mpsc::Sender<ApprovalResolution>,
    command_tx: mpsc::Sender<ChannelCommand>,
    approval_registry: Arc<ApprovalRegistry>,
    bot_user_id: Arc<RwLock<Option<u64>>>,
    http_client: reqwest::Client,
    transcriber: Option<Arc<dyn Transcriber>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: Context, ready: serenity::model::gateway::Ready) {
        tracing::info!(
            bot_user = %ready.user.name,
            guild_count = ready.guilds.len(),
            "discord bot connected and ready"
        );
        let mut bot_user_id = self.bot_user_id.write().await;
        *bot_user_id = Some(ready.user.id.get());

        if let Err(err) = Command::set_global_commands(
            &ctx.http,
            vec![
                CreateCommand::new("new").description("Start a fresh conversation session"),
                CreateCommand::new("status").description("Show bot and session status"),
                CreateCommand::new("stop").description("Stop running background tasks"),
                CreateCommand::new("tools").description("List available agent tools"),
            ],
        )
        .await
        {
            tracing::warn!(
                status = "failed",
                error_kind = "register_slash_commands",
                error = %err,
                "discord slash command registration failed"
            );
        }
    }

    async fn message(&self, _ctx: Context, msg: DiscordMessage) {
        if msg.author.bot {
            return;
        }

        let inbound_kind = message_kind(&msg);
        let transcription_outcome = match self.transcribe_audio_attachments(&msg).await {
            Ok(Some(content)) => InboundTranscriptionOutcome::Transcript(content),
            Ok(None) => InboundTranscriptionOutcome::Unavailable,
            Err(err) => {
                tracing::warn!(
                    status = "degraded",
                    error_kind = "audio_transcription",
                    error = %err,
                    message_id = %msg.id.get(),
                    "discord audio transcription failed"
                );
                InboundTranscriptionOutcome::Unavailable
            }
        };
        let Some(content) =
            compose_inbound_content(&msg.content, inbound_kind, &transcription_outcome)
        else {
            return;
        };
        if inbound_kind == InboundMessageKind::Voice
            && matches!(
                transcription_outcome,
                InboundTranscriptionOutcome::Unavailable
            )
            && msg.content.trim().is_empty()
        {
            tracing::warn!(
                status = "degraded",
                error_kind = "voice_transcription_unavailable",
                message_id = %msg.id.get(),
                "discord voice message routed with fallback placeholder"
            );
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
            content,
            kind: inbound_kind,
        };

        tracing::debug!(status = "received", "discord inbound received");
        if let Err(err) = self.inbound_tx.send(inbound).await {
            tracing::warn!(status = "dropped", error_kind = "queue_closed", error = %err, "discord inbound queue closed");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => self.handle_slash_command(&ctx, command).await,
            Interaction::Component(component) => {
                self.handle_approval_interaction(&ctx, component).await;
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn supports_message_editing(&self) -> bool {
        true
    }

    fn message_char_limit(&self) -> Option<usize> {
        Some(2_000)
    }

    fn supports_approval_resolution(&self) -> bool {
        true
    }

    fn supports_commands(&self) -> bool {
        true
    }

    async fn begin_stream(
        &self,
        channel_id: &str,
    ) -> Result<Box<dyn crate::channels::ChannelStream>, FrameworkError> {
        let parsed = parse_channel_id(channel_id)?;
        Ok(Box::new(super::discord_stream::DiscordChannelStream::new(
            Arc::clone(&self.http),
            &parsed.get().to_string(),
            self.message_char_limit().unwrap_or(2_000),
        )))
    }

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

    async fn send_message_with_attachment(
        &self,
        channel_id: &str,
        content: &str,
        attachment_bytes: Vec<u8>,
        attachment_filename: String,
    ) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let mut message = CreateMessage::new().add_file(CreateAttachment::bytes(
            attachment_bytes,
            attachment_filename,
        ));
        if !content.is_empty() {
            message = message.content(content);
        }
        channel_id
            .send_message(&self.http, message)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord attachment send failed: {e}")))?;
        Ok(())
    }

    async fn send_voice_message(
        &self,
        channel_id: &str,
        voice_message: OutboundVoiceMessage,
    ) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let endpoint = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            channel_id.get()
        );
        let payload = json!({
            "flags": serenity::model::channel::MessageFlags::IS_VOICE_MESSAGE.bits(),
            "attachments": [{
                "id": "0",
                "filename": voice_message.attachment_filename,
                "duration_secs": voice_message.duration_secs,
                "waveform": voice_message.waveform,
            }]
        });
        let audio_part = Part::bytes(voice_message.audio_bytes)
            .file_name("voice-message.ogg")
            .mime_str("audio/ogg")
            .map_err(|err| {
                FrameworkError::Tool(format!(
                    "failed to prepare discord voice message mime metadata: {err}"
                ))
            })?;
        let form = Form::new()
            .text("payload_json", payload.to_string())
            .part("files[0]", audio_part);
        let response = self
            .api_client
            .post(endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bot {}", self.token),
            )
            .multipart(form)
            .send()
            .await
            .map_err(|err| {
                FrameworkError::Tool(format!("discord voice message send failed: {err}"))
            })?;
        response.error_for_status().map_err(|err| {
            FrameworkError::Tool(format!("discord voice message send failed: {err}"))
        })?;
        Ok(())
    }

    async fn send_message_with_id(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Option<String>, FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        tracing::debug!(status = "sending", "discord send");
        let message = channel_id
            .say(&self.http, content)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord send failed: {e}")))?;
        tracing::debug!(status = "completed", message_id = %message.id.get(), "discord send");
        Ok(Some(message.id.get().to_string()))
    }

    async fn send_approval_request(
        &self,
        channel_id: &str,
        request: &PendingApprovalRequest,
    ) -> Result<Option<String>, FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let message = CreateMessage::new()
            .content(render_approval_request(request))
            .components(vec![CreateActionRow::Buttons(vec![
                CreateButton::new(format!(
                    "simpleclaw:approval:{}:approve",
                    request.approval_id
                ))
                .label("Approve")
                .style(ButtonStyle::Success),
                CreateButton::new(format!("simpleclaw:approval:{}:deny", request.approval_id))
                    .label("Deny")
                    .style(ButtonStyle::Danger),
            ])]);
        let message = channel_id
            .send_message(&self.http, message)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord approval send failed: {e}")))?;
        Ok(Some(message.id.get().to_string()))
    }

    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let message_id = parse_message_id(message_id)?;
        channel_id
            .edit_message(&self.http, message_id, EditMessage::new().content(content))
            .await
            .map_err(|e| FrameworkError::Config(format!("discord edit failed: {e}")))?;
        Ok(())
    }

    async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), FrameworkError> {
        let channel_id = parse_channel_id(channel_id)?;
        let message_id = parse_message_id(message_id)?;
        channel_id
            .delete_message(&self.http, message_id)
            .await
            .map_err(|e| FrameworkError::Config(format!("discord delete failed: {e}")))?;
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

    async fn listen_for_approval(&self) -> Result<ApprovalResolution, FrameworkError> {
        let mut rx = self.approval_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| FrameworkError::Config("discord approval channel closed".to_owned()))
    }

    async fn listen_for_command(&self) -> Result<ChannelCommand, FrameworkError> {
        let mut rx = self.command_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| FrameworkError::Config("discord command channel closed".to_owned()))
    }
}

impl DiscordHandler {
    async fn transcribe_audio_attachments(
        &self,
        msg: &DiscordMessage,
    ) -> Result<Option<String>, FrameworkError> {
        let Some(transcriber) = self.transcriber.as_ref() else {
            return Ok(None);
        };

        let mut transcripts = Vec::new();
        for attachment in msg.attachments.iter().filter(is_audio_attachment) {
            let response = self
                .http_client
                .get(&attachment.url)
                .send()
                .await
                .map_err(|err| {
                    FrameworkError::Tool(format!(
                        "failed to download discord attachment '{}': {err}",
                        attachment.filename
                    ))
                })?;
            let response = response.error_for_status().map_err(|err| {
                FrameworkError::Tool(format!(
                    "discord attachment download failed for '{}': {err}",
                    attachment.filename
                ))
            })?;
            let bytes = response.bytes().await.map_err(|err| {
                FrameworkError::Tool(format!(
                    "failed to read discord attachment '{}': {err}",
                    attachment.filename
                ))
            })?;
            let transcript = transcriber
                .transcribe(bytes.as_ref(), &attachment.filename)
                .await?;
            transcripts.push(transcript);
        }

        if transcripts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(transcripts.join("\n\n")))
        }
    }

    async fn handle_slash_command(&self, ctx: &Context, command: CommandInteraction) {
        let Some(kind) = slash_command_kind(&command.data.name) else {
            return;
        };

        if let Err(err) = command.defer_ephemeral(&ctx.http).await {
            tracing::warn!(
                status = "failed",
                error_kind = "interaction_defer",
                error = %err,
                command = %command.data.name,
                "discord slash command defer failed"
            );
            return;
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = ChannelCommand {
            kind,
            source: crate::config::GatewayChannelKind::Discord,
            channel_id: command.channel_id.get().to_string(),
            guild_id: command.guild_id.map(|id| id.get().to_string()),
            user_id: command.user.id.get().to_string(),
            is_dm: command.guild_id.is_none(),
            reply_tx,
        };

        let response = if let Err(err) = self.command_tx.send(request).await {
            tracing::warn!(
                status = "failed",
                error_kind = "command_queue_closed",
                error = %err,
                command = %command.data.name,
                "discord slash command queue closed"
            );
            fallback_response(kind, "Failed to dispatch the command.")
        } else {
            match timeout(Duration::from_secs(10), reply_rx).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) => fallback_response(kind, "The command response channel closed."),
                Err(_) => fallback_response(kind, "The command timed out."),
            }
        };

        if let Err(err) = command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content(truncate_discord_response(response.message())),
            )
            .await
        {
            tracing::warn!(
                status = "failed",
                error_kind = "interaction_edit",
                error = %err,
                command = %command.data.name,
                "discord slash command response edit failed"
            );
        }
    }

    async fn handle_approval_interaction(&self, ctx: &Context, component: ComponentInteraction) {
        let Some((approval_id, decision)) = parse_approval_custom_id(&component) else {
            return;
        };
        let actor_user_id = component.user.id.get().to_string();
        let pending = self.approval_registry.pending_request(&approval_id).await;

        match evaluate_approval_interaction(
            pending,
            actor_user_id.clone(),
            component.channel_id.get().to_string(),
            approval_id,
            decision,
        ) {
            ApprovalInteractionResult::Inactive { message } => {
                if let Err(err) = component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(message)
                                .ephemeral(true),
                        ),
                    )
                    .await
                {
                    tracing::warn!(status = "failed", error_kind = "interaction_ack", error = %err, "discord inactive approval interaction ack failed");
                }
            }
            ApprovalInteractionResult::Unauthorized {
                message,
                approval_id,
                channel_id,
                user_id,
            } => {
                tracing::warn!(
                    status = "ignored",
                    reason = "requesting_user_mismatch",
                    approval_id,
                    channel_id,
                    user_id,
                    "discord approval interaction ignored"
                );
                if let Err(err) = component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(message)
                                .ephemeral(true),
                        ),
                    )
                    .await
                {
                    tracing::warn!(status = "failed", error_kind = "interaction_ack", error = %err, "discord unauthorized approval interaction ack failed");
                }
            }
            ApprovalInteractionResult::Authorized {
                resolution,
                rendered_message,
            } => {
                if let Err(err) = self.approval_tx.send(resolution).await {
                    tracing::warn!(status = "dropped", error_kind = "approval_queue_closed", error = %err, "discord approval queue closed");
                    if let Err(ack_err) = component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content(
                                        "Failed to record the approval response. Please try again.",
                                    )
                                    .ephemeral(true),
                            ),
                        )
                        .await
                    {
                        tracing::warn!(status = "failed", error_kind = "interaction_ack", error = %ack_err, "discord approval queue failure ack failed");
                    }
                    return;
                }
                if let Err(err) = component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .content(rendered_message)
                                .components(Vec::new()),
                        ),
                    )
                    .await
                {
                    tracing::warn!(status = "failed", error_kind = "interaction_ack", error = %err, "discord approval interaction ack failed");
                }
            }
        }
    }
}

enum InboundTranscriptionOutcome {
    Transcript(String),
    Unavailable,
}

fn compose_inbound_content(
    text: &str,
    inbound_kind: InboundMessageKind,
    transcription: &InboundTranscriptionOutcome,
) -> Option<String> {
    let text = text.trim();
    let transcription = match transcription {
        InboundTranscriptionOutcome::Transcript(value) => {
            let value = value.trim();
            (!value.is_empty()).then_some(value)
        }
        InboundTranscriptionOutcome::Unavailable => None,
    };
    match (text.is_empty(), transcription) {
        (true, None) if inbound_kind == InboundMessageKind::Voice => {
            Some(VOICE_TRANSCRIPTION_UNAVAILABLE_PLACEHOLDER.to_owned())
        }
        (true, None) => None,
        (false, None) => Some(text.to_owned()),
        (true, Some(transcription)) => Some(transcription.to_owned()),
        (false, Some(transcription)) => Some(format!("{text}\n\n{transcription}")),
    }
}

fn message_kind(msg: &DiscordMessage) -> InboundMessageKind {
    if msg
        .flags
        .unwrap_or_default()
        .contains(serenity::model::channel::MessageFlags::IS_VOICE_MESSAGE)
    {
        InboundMessageKind::Voice
    } else {
        InboundMessageKind::Text
    }
}

fn is_audio_attachment(attachment: &&serenity::model::channel::Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .map(|content_type| content_type.starts_with("audio/"))
        .unwrap_or(false)
        || has_audio_extension(&attachment.filename)
}

fn has_audio_extension(filename: &str) -> bool {
    let Some(extension) = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
    else {
        return false;
    };
    match extension.to_ascii_lowercase().as_str() {
        "ogg" | "mp3" | "wav" | "m4a" | "webm" => true,
        _ => false,
    }
}

fn slash_command_kind(name: &str) -> Option<ChannelCommandKind> {
    match name {
        "new" => Some(ChannelCommandKind::NewSession),
        "status" => Some(ChannelCommandKind::Status),
        "stop" => Some(ChannelCommandKind::Stop),
        "tools" => Some(ChannelCommandKind::Tools),
        _ => None,
    }
}

fn fallback_response(kind: ChannelCommandKind, message: &str) -> CommandResponse {
    match kind {
        ChannelCommandKind::NewSession => CommandResponse::NewSession {
            message: message.to_owned(),
        },
        ChannelCommandKind::Status => CommandResponse::Status {
            message: message.to_owned(),
        },
        ChannelCommandKind::Stop => CommandResponse::Stop {
            killed_count: 0,
            message: message.to_owned(),
        },
        ChannelCommandKind::Tools => CommandResponse::Tools {
            message: message.to_owned(),
        },
    }
}

fn truncate_discord_response(message: &str) -> String {
    const DISCORD_LIMIT: usize = 2_000;
    let count = message.chars().count();
    if count <= DISCORD_LIMIT {
        return message.to_owned();
    }

    let truncated: String = message.chars().take(DISCORD_LIMIT - 3).collect();
    format!("{truncated}...")
}

fn render_approval_request(request: &PendingApprovalRequest) -> String {
    format!(
        "**Approval required**\n{} wants to run `{}` outside the sandbox.\n\nRequested by: {}\nAction: `{}`\nWhy: {}",
        request.agent_name,
        request.tool_name,
        format_discord_user(&request.requesting_user_id),
        request.action_summary,
        request.reason
    )
}

fn render_resolved_approval_request(request: &PendingApprovalRequest) -> String {
    let _ = request;
    "**Approval request closed**".to_owned()
}

fn format_discord_user(user_id: &str) -> String {
    if user_id.chars().all(|ch| ch.is_ascii_digit()) {
        format!("<@{user_id}>")
    } else {
        user_id.to_owned()
    }
}

enum ApprovalInteractionResult {
    Inactive {
        message: &'static str,
    },
    Unauthorized {
        message: &'static str,
        approval_id: String,
        channel_id: String,
        user_id: String,
    },
    Authorized {
        resolution: ApprovalResolution,
        rendered_message: String,
    },
}

fn evaluate_approval_interaction(
    pending: Option<PendingApprovalRequest>,
    actor_user_id: String,
    channel_id: String,
    approval_id: String,
    decision: ApprovalDecision,
) -> ApprovalInteractionResult {
    let Some(request) = pending else {
        return ApprovalInteractionResult::Inactive {
            message: "This approval request expired.",
        };
    };

    if request.requesting_user_id != actor_user_id {
        return ApprovalInteractionResult::Unauthorized {
            message: "Only the requester can use these buttons.",
            approval_id,
            channel_id,
            user_id: actor_user_id,
        };
    }

    ApprovalInteractionResult::Authorized {
        resolution: ApprovalResolution {
            approval_id,
            decision,
            channel_id,
            user_id: actor_user_id,
        },
        rendered_message: render_resolved_approval_request(&request),
    }
}

fn parse_approval_custom_id(
    component: &ComponentInteraction,
) -> Option<(String, ApprovalDecision)> {
    let mut parts = component.data.custom_id.split(':');
    let prefix = parts.next()?;
    let kind = parts.next()?;
    let approval_id = parts.next()?.to_owned();
    let decision = parts.next()?;
    if prefix != "simpleclaw" || kind != "approval" || parts.next().is_some() {
        return None;
    }
    let decision = match decision {
        "approve" => ApprovalDecision::Approved,
        "deny" => ApprovalDecision::Denied,
        _ => return None,
    };
    Some((approval_id, decision))
}

#[cfg(test)]
mod approval_tests {
    use super::{ApprovalInteractionResult, evaluate_approval_interaction};
    use crate::approval::{ApprovalDecision, PendingApprovalRequest};

    fn pending_request() -> PendingApprovalRequest {
        PendingApprovalRequest {
            approval_id: "approval-1".to_owned(),
            agent_id: "agent-1".to_owned(),
            agent_name: "Agent One".to_owned(),
            session_id: "sess-1".to_owned(),
            requesting_user_id: "user-1".to_owned(),
            tool_name: "read".to_owned(),
            execution_kind: "preflight_escalation".to_owned(),
            capability: "read".to_owned(),
            reason: "outside sandbox".to_owned(),
            action_summary: "/tmp/secret.txt".to_owned(),
            diagnostic: "blocked".to_owned(),
        }
    }

    #[test]
    fn approval_interaction_rejects_non_requesting_user() {
        let outcome = evaluate_approval_interaction(
            Some(pending_request()),
            "user-2".to_owned(),
            "chan-1".to_owned(),
            "approval-1".to_owned(),
            ApprovalDecision::Approved,
        );

        match outcome {
            ApprovalInteractionResult::Unauthorized {
                approval_id,
                user_id,
                ..
            } => {
                assert_eq!(approval_id, "approval-1");
                assert_eq!(user_id, "user-2");
            }
            _ => panic!("expected unauthorized interaction"),
        }
    }

    #[test]
    fn approval_interaction_accepts_requesting_user() {
        let outcome = evaluate_approval_interaction(
            Some(pending_request()),
            "user-1".to_owned(),
            "chan-1".to_owned(),
            "approval-1".to_owned(),
            ApprovalDecision::Denied,
        );

        match outcome {
            ApprovalInteractionResult::Authorized {
                resolution,
                rendered_message,
            } => {
                assert_eq!(resolution.approval_id, "approval-1");
                assert_eq!(resolution.user_id, "user-1");
                assert_eq!(rendered_message, "**Approval request closed**");
            }
            _ => panic!("expected authorized interaction"),
        }
    }

    #[test]
    fn approval_interaction_marks_missing_request_inactive() {
        let outcome = evaluate_approval_interaction(
            None,
            "user-1".to_owned(),
            "chan-1".to_owned(),
            "approval-1".to_owned(),
            ApprovalDecision::Approved,
        );

        match outcome {
            ApprovalInteractionResult::Inactive { message } => {
                assert_eq!(message, "This approval request expired.");
            }
            _ => panic!("expected inactive interaction"),
        }
    }

    #[test]
    fn render_approval_request_mentions_numeric_discord_user_ids() {
        let mut request = pending_request();
        request.requesting_user_id = "123456789".to_owned();

        let rendered = super::render_approval_request(&request);

        assert!(rendered.contains("Agent One wants to run `read` outside the sandbox."));
        assert!(rendered.contains("Requested by: <@123456789>"));
    }

    #[test]
    fn render_approval_request_hides_internal_metadata() {
        let rendered = super::render_approval_request(&pending_request());

        assert!(!rendered.contains("agent-1"));
        assert!(!rendered.contains("preflight_escalation"));
        assert!(!rendered.contains("read` via"));
        assert!(!rendered.contains("blocked"));
    }
}

pub(crate) fn parse_channel_id(raw: &str) -> Result<ChannelId, FrameworkError> {
    let id: u64 = raw
        .parse()
        .map_err(|_| FrameworkError::Tool(format!("invalid discord channel id: {raw}")))?;
    Ok(ChannelId::new(id))
}

pub(crate) fn parse_message_id(raw: &str) -> Result<MessageId, FrameworkError> {
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

    use super::{
        InboundTranscriptionOutcome, VOICE_TRANSCRIPTION_UNAVAILABLE_PLACEHOLDER,
        collect_bot_reaction_types, compose_inbound_content, parse_reaction_type,
    };
    use crate::channels::InboundMessageKind;
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
        let parsed = parse_reaction_type("<:party:123456>").expect("custom emoji should parse");

        assert_eq!(
            parsed,
            ReactionType::Custom {
                animated: false,
                id: EmojiId::new(123456),
                name: Some("party".to_owned()),
            }
        );
    }

    #[test]
    fn compose_inbound_content_routes_voice_transcript() {
        let content = compose_inbound_content(
            "",
            InboundMessageKind::Voice,
            &InboundTranscriptionOutcome::Transcript("hello there".to_owned()),
        );

        assert_eq!(content.as_deref(), Some("hello there"));
    }

    #[test]
    fn compose_inbound_content_routes_voice_placeholder_when_transcription_unavailable() {
        let content = compose_inbound_content(
            "",
            InboundMessageKind::Voice,
            &InboundTranscriptionOutcome::Unavailable,
        );

        assert_eq!(
            content.as_deref(),
            Some(VOICE_TRANSCRIPTION_UNAVAILABLE_PLACEHOLDER)
        );
    }

    #[test]
    fn compose_inbound_content_preserves_text_when_transcription_unavailable() {
        let content = compose_inbound_content(
            "hello",
            InboundMessageKind::Text,
            &InboundTranscriptionOutcome::Unavailable,
        );

        assert_eq!(content.as_deref(), Some("hello"));
    }

    #[test]
    fn compose_inbound_content_drops_empty_non_voice_without_transcript() {
        let content = compose_inbound_content(
            "",
            InboundMessageKind::Text,
            &InboundTranscriptionOutcome::Unavailable,
        );

        assert!(content.is_none());
    }
}
