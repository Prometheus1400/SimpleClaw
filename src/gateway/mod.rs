use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tracing::{Instrument, info_span};

use crate::agent::AgentDirectory;
use crate::approval::{ApprovalRegistry, PendingApprovalRequest};
use crate::channels::{ApprovalResolution, Channel, InboundMessage};
use crate::config::{ChannelOutputMode, GatewayChannelKind, RoutingConfig};
use crate::error::FrameworkError;
use crate::run::session::SessionWorkerCoordinator;
use crate::tools::ToolExecEnv;
use crate::tools::{AsyncToolRunManager, AsyncToolRunStatus};

mod command;
mod policy;
mod router;
mod session;
mod transport;

pub(crate) use command::{ChannelCommand, ChannelCommandKind, CommandResponse};
pub(crate) use session::SessionStore;
use session::build_session_scope_key;

#[derive(Clone)]
pub struct Gateway {
    channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
    output_modes: HashMap<GatewayChannelKind, ChannelOutputMode>,
    inbound_policy: RoutingConfig,
    agents: Arc<AgentDirectory>,
    async_tool_runs: Arc<AsyncToolRunManager>,
    session_coordinator: SessionWorkerCoordinator<InboundMessage>,
    session_store: SessionStore,
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
        Self::with_runtime_dependencies(
            channels,
            output_modes,
            inbound_policy,
            Arc::new(AgentDirectory::new(HashMap::new(), HashMap::new())),
            Arc::new(AsyncToolRunManager::new()),
            SessionWorkerCoordinator::new(Duration::from_secs(300)),
            SessionStore::in_memory().expect("in-memory session store should open"),
        )
    }

    pub(crate) fn with_runtime_dependencies(
        channels: HashMap<GatewayChannelKind, Arc<dyn Channel>>,
        output_modes: HashMap<GatewayChannelKind, ChannelOutputMode>,
        inbound_policy: RoutingConfig,
        agents: Arc<AgentDirectory>,
        async_tool_runs: Arc<AsyncToolRunManager>,
        session_coordinator: SessionWorkerCoordinator<InboundMessage>,
        session_store: SessionStore,
    ) -> Self {
        Self {
            channels,
            output_modes,
            inbound_policy,
            agents,
            async_tool_runs,
            session_coordinator,
            session_store,
        }
    }

    pub fn start(
        &self,
        inbound_tx: mpsc::Sender<InboundMessage>,
        approvals: Arc<ApprovalRegistry>,
    ) -> GatewayListeners {
        let mut tasks = Vec::with_capacity(self.channels.len() * 3);
        for (kind, channel) in &self.channels {
            let kind = *kind;
            let supports_approval_resolution = channel.supports_approval_resolution();
            let supports_commands = channel.supports_commands();
            let channel = Arc::clone(channel);
            let inbound_channel = Arc::clone(&channel);
            let inbound_tx = inbound_tx.clone();
            let inbound_policy = self.inbound_policy.clone();
            let session_store = self.session_store.clone();
            let listener_span = info_span!("gateway.listen");
            tasks.push(tokio::spawn(
                async move {
                    loop {
                        match inbound_channel.listen().await {
                            Ok(inbound) => {
                                let routed = router::route_inbound(
                                    kind,
                                    inbound,
                                    &inbound_policy,
                                    &session_store,
                                )
                                .await;
                                let routed = match routed {
                                    Ok(inbound) => inbound,
                                    Err(err) => {
                                        tracing::error!(
                                            status = "retrying",
                                            error_kind = "session_resolve",
                                            error = %err,
                                            "failed to resolve active session"
                                        );
                                        sleep(Duration::from_secs(1)).await;
                                        continue;
                                    }
                                };
                                let Some(inbound) = routed else {
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

            if supports_approval_resolution {
                let channel = Arc::clone(&channel);
                let approvals = Arc::clone(&approvals);
                let listener_span = info_span!("gateway.listen.approval");
                tasks.push(tokio::spawn(
                    async move {
                        loop {
                            match channel.listen_for_approval().await {
                                Ok(ApprovalResolution {
                                    approval_id,
                                    decision,
                                    channel_id,
                                    user_id,
                                }) => {
                                    let resolved =
                                        approvals.resolve(&approval_id, &user_id, decision).await;
                                    if !resolved {
                                        let known =
                                            approvals.pending_request(&approval_id).await.is_some();
                                        if known {
                                            tracing::warn!(
                                                status = "ignored",
                                                reason = "requesting_user_mismatch",
                                                approval_id,
                                                channel_id,
                                                user_id,
                                                "approval resolution ignored"
                                            );
                                        } else {
                                            tracing::warn!(
                                                status = "dropped",
                                                reason = "unknown_approval_id",
                                                approval_id,
                                                channel_id,
                                                user_id,
                                                "approval resolution dropped"
                                            );
                                        }
                                    } else {
                                        tracing::debug!(
                                            status = "resolved",
                                            approval_id,
                                            channel_id,
                                            user_id,
                                            "approval resolution accepted"
                                        );
                                    }
                                }
                                Err(err) => {
                                    tracing::error!(
                                        status = "retrying",
                                        error_kind = "approval_listen",
                                        error = %err,
                                        "approval listener failed"
                                    );
                                    sleep(Duration::from_secs(1)).await;
                                }
                            }
                        }
                    }
                    .instrument(listener_span),
                ));
            }

            if supports_commands {
                let channel = Arc::clone(&channel);
                let gateway = self.clone();
                let listener_span = info_span!("gateway.listen.command");
                tasks.push(tokio::spawn(
                    async move {
                        loop {
                            match channel.listen_for_command().await {
                                Ok(command) => gateway.execute_command(command).await,
                                Err(err) => {
                                    tracing::error!(
                                        status = "retrying",
                                        error_kind = "command_listen",
                                        error = %err,
                                        "command listener failed"
                                    );
                                    sleep(Duration::from_secs(1)).await;
                                }
                            }
                        }
                    }
                    .instrument(listener_span),
                ));
            }
        }

        GatewayListeners { tasks }
    }

    pub async fn execute_command(&self, cmd: ChannelCommand) {
        let kind = cmd.kind;
        let response = match self.resolve_command_target(&cmd).await {
            Err(err) => self.error_command_response(kind, &err),
            Ok(Some((agent_id, scope_key, session_key))) => {
                let Some(config) = self.agents.config(&agent_id) else {
                    return self
                        .send_command_response(
                            cmd,
                            self.unknown_agent_command_response(kind, &agent_id),
                        )
                        .await;
                };

                if !ToolExecEnv::owner_allowed(&cmd.user_id, &config.owner_ids) {
                    return self
                        .send_command_response(cmd, self.owner_denied_command_response(kind))
                        .await;
                }

                match kind {
                    ChannelCommandKind::NewSession => {
                        let rotated = match self.session_store.rotate(&scope_key).await {
                            Ok(rotated) => rotated,
                            Err(err) => {
                                return self
                                    .send_command_response(
                                        cmd,
                                        self.error_command_response(kind, &err),
                                    )
                                    .await;
                            }
                        };
                        self.session_coordinator
                            .remove(&rotated.previous_session_id)
                            .await;
                        CommandResponse::NewSession {
                            message: format!(
                                "Started a fresh session for `{agent_id}`. New session: `{}`.",
                                rotated.session_id
                            ),
                        }
                    }
                    ChannelCommandKind::Status => {
                        self.status_command_response(&agent_id, &scope_key, &session_key)
                            .await
                    }
                    ChannelCommandKind::Stop => {
                        self.stop_command_response(&agent_id, &session_key).await
                    }
                    ChannelCommandKind::Tools => self.tools_command_response(&agent_id),
                }
            }
            Ok(None) => self.denied_command_response(kind),
        };

        self.send_command_response(cmd, response).await;
    }

    async fn send_command_response(&self, cmd: ChannelCommand, response: CommandResponse) {
        let _ = cmd.reply_tx.send(response);
    }

    pub async fn send_message(
        &self,
        inbound: &InboundMessage,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel.send_message(&inbound.channel_id, content).await
    }

    pub async fn send_approval_request(
        &self,
        inbound: &InboundMessage,
        request: &PendingApprovalRequest,
    ) -> Result<Option<String>, FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel
            .send_approval_request(&inbound.channel_id, request)
            .await
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

    pub fn message_char_limit(
        &self,
        inbound: &InboundMessage,
    ) -> Result<Option<usize>, FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        Ok(channel.message_char_limit())
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

    pub async fn delete_message(
        &self,
        inbound: &InboundMessage,
        message_id: &str,
    ) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel
            .delete_message(&inbound.channel_id, message_id)
            .await
    }

    pub async fn broadcast_typing(&self, inbound: &InboundMessage) -> Result<(), FrameworkError> {
        let channel = transport::channel_for_source(&self.channels, inbound.source_channel)?;
        channel.broadcast_typing(&inbound.channel_id).await
    }

    async fn resolve_command_target(
        &self,
        cmd: &ChannelCommand,
    ) -> Result<Option<(String, String, String)>, FrameworkError> {
        let inbound = crate::channels::ChannelInbound {
            message_id: format!("command:{}", cmd.kind.as_str()),
            channel_id: cmd.channel_id.clone(),
            guild_id: cmd.guild_id.clone(),
            is_dm: cmd.is_dm,
            user_id: cmd.user_id.clone(),
            username: "system".to_owned(),
            mentioned_bot: true,
            content: format!("/{}", cmd.kind.as_str()),
        };
        let Some((agent_id, _)) = router::resolve_route(cmd.source, &inbound, &self.inbound_policy)
        else {
            return Ok(None);
        };
        let scope_key = build_session_scope_key(&agent_id, cmd.is_dm, cmd.source, &cmd.channel_id);
        let session_key = self.session_store.current_or_create(&scope_key).await?;
        Ok(Some((agent_id, scope_key, session_key)))
    }

    fn denied_command_response(&self, kind: ChannelCommandKind) -> CommandResponse {
        let message = "That command is not allowed in this channel.";
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

    fn owner_denied_command_response(&self, kind: ChannelCommandKind) -> CommandResponse {
        let message = "That command is only available to configured owners.";
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

    fn unknown_agent_command_response(
        &self,
        kind: ChannelCommandKind,
        agent_id: &str,
    ) -> CommandResponse {
        let message = format!("Agent `{agent_id}` is not configured.");
        match kind {
            ChannelCommandKind::NewSession => CommandResponse::NewSession { message },
            ChannelCommandKind::Status => CommandResponse::Status { message },
            ChannelCommandKind::Stop => CommandResponse::Stop {
                killed_count: 0,
                message,
            },
            ChannelCommandKind::Tools => CommandResponse::Tools { message },
        }
    }

    fn error_command_response(
        &self,
        kind: ChannelCommandKind,
        err: &FrameworkError,
    ) -> CommandResponse {
        let message = format!("Failed to resolve session state: {err}");
        match kind {
            ChannelCommandKind::NewSession => CommandResponse::NewSession { message },
            ChannelCommandKind::Status => CommandResponse::Status { message },
            ChannelCommandKind::Stop => CommandResponse::Stop {
                killed_count: 0,
                message,
            },
            ChannelCommandKind::Tools => CommandResponse::Tools { message },
        }
    }

    async fn status_command_response(
        &self,
        agent_id: &str,
        scope_key: &str,
        session_key: &str,
    ) -> CommandResponse {
        let Some(config) = self.agents.config(agent_id) else {
            return CommandResponse::Status {
                message: format!("Agent `{agent_id}` is not configured."),
            };
        };

        let runs = self
            .async_tool_runs
            .list_for_session(agent_id, session_key)
            .await;
        let running = runs
            .iter()
            .filter(|run| run.status == AsyncToolRunStatus::Running)
            .count();

        let mut message = format!(
            "Agent: {} (`{}`)\nScope: `{}`\nSession: `{}`\nBackground runs: {} active / {} total",
            config.agent_name,
            agent_id,
            scope_key,
            session_key,
            running,
            runs.len()
        );

        if !runs.is_empty() {
            let lines = runs
                .iter()
                .take(5)
                .map(|run| format!("- `{}` {} {}", run.run_id, run.status.as_str(), run.summary))
                .collect::<Vec<_>>()
                .join("\n");
            message.push_str("\nRecent runs:\n");
            message.push_str(&lines);
        }

        CommandResponse::Status { message }
    }

    async fn stop_command_response(&self, agent_id: &str, session_key: &str) -> CommandResponse {
        let runs = self
            .async_tool_runs
            .list_for_session(agent_id, session_key)
            .await;
        let mut killed_count = 0usize;

        for run in runs {
            if run.status != AsyncToolRunStatus::Running {
                continue;
            }
            if self
                .async_tool_runs
                .kill_for_session(&run.run_id, agent_id, session_key)
                .await
                .is_ok()
            {
                killed_count += 1;
            }
        }

        let message = if killed_count == 0 {
            "No running background tasks were found.".to_owned()
        } else {
            format!("Stopped {killed_count} background task(s).")
        };

        CommandResponse::Stop {
            killed_count,
            message,
        }
    }

    fn tools_command_response(&self, agent_id: &str) -> CommandResponse {
        let Some(config) = self.agents.config(agent_id) else {
            return CommandResponse::Tools {
                message: format!("Agent `{agent_id}` is not configured."),
            };
        };

        let definitions = config.tool_registry.definitions();
        if definitions.is_empty() {
            return CommandResponse::Tools {
                message: format!("Agent `{agent_id}` has no tools configured."),
            };
        }

        let message = definitions
            .into_iter()
            .map(|tool| format!("- `{}`: {}", tool.name, tool.description))
            .collect::<Vec<_>>()
            .join("\n");

        CommandResponse::Tools { message }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio::time::Duration;

    use crate::agent::{AgentDirectory, AgentRuntimeConfig};
    use crate::approval::ApprovalRegistry;
    use crate::channels::{Channel, ChannelInbound};
    use crate::config::{
        AgentInnerConfig, ChannelOutputMode, ExecutionDefaultsConfig, GatewayChannelKind,
        RoutingConfig,
    };
    use crate::error::FrameworkError;
    use crate::run::session::SessionWorkerCoordinator;
    use crate::tools::{AsyncToolRunManager, default_factory};

    use super::{ChannelCommand, ChannelCommandKind, Gateway, SessionStore};

    struct SingleInboundChannel {
        inbound: Mutex<Option<ChannelInbound>>,
    }

    fn test_gateway(owner_ids: Vec<String>) -> Gateway {
        let mut agent_config = AgentInnerConfig::default();
        agent_config.tools = agent_config.tools.with_disabled(&["cron"]);
        let tool_registry = default_factory()
            .build_registry(&agent_config.tools, &[])
            .expect("tool registry should build");
        let runtime_config = AgentRuntimeConfig {
            agent_id: "default".to_owned(),
            agent_name: "Default".to_owned(),
            provider_key: "default".to_owned(),
            effective_execution: ExecutionDefaultsConfig::default(),
            owner_ids,
            agent_config,
            tool_registry,
            persona_root: PathBuf::from("/tmp/simpleclaw-gateway-test-persona"),
            workspace_root: PathBuf::from("/tmp/simpleclaw-gateway-test-workspace"),
            app_base_dir: PathBuf::from("/tmp/simpleclaw-gateway-test-app"),
            system_prompt: "base prompt".to_owned(),
        };

        Gateway::with_runtime_dependencies(
            HashMap::new(),
            HashMap::new(),
            RoutingConfig::default(),
            Arc::new(AgentDirectory::new(
                HashMap::from([("default".to_owned(), runtime_config)]),
                HashMap::new(),
            )),
            Arc::new(AsyncToolRunManager::new()),
            SessionWorkerCoordinator::new(Duration::from_secs(300)),
            SessionStore::in_memory().expect("session store should open"),
        )
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
        let _listeners = gateway.start(tx, Arc::new(ApprovalRegistry::new()));
        let next = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("gateway should emit normalized inbound")
            .expect("inbound should decode");
        assert_eq!(next.source_channel, GatewayChannelKind::Discord);
        assert_eq!(next.session_key, "agent:default:discord:123:session:1");
        assert_eq!(next.source_message_id.as_deref(), Some("321"));
    }

    #[tokio::test]
    async fn gateway_exposes_channel_message_limit() {
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
        struct LimitedChannel;

        #[async_trait]
        impl Channel for LimitedChannel {
            fn message_char_limit(&self) -> Option<usize> {
                Some(2_000)
            }

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
                pending::<Result<ChannelInbound, FrameworkError>>().await
            }
        }

        let mut channels = HashMap::new();
        channels.insert(
            GatewayChannelKind::Discord,
            Arc::new(LimitedChannel) as Arc<dyn Channel>,
        );
        let gateway = Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        );
        let _listeners = gateway.start(mpsc::channel(1).0, Arc::new(ApprovalRegistry::new()));
        let routed = crate::gateway::router::route_inbound(
            GatewayChannelKind::Discord,
            inbound,
            &RoutingConfig::default(),
            &SessionStore::in_memory().expect("session store should open"),
        )
        .await
        .expect("routing should succeed")
        .expect("inbound should route");

        assert_eq!(gateway.message_char_limit(&routed).unwrap(), Some(2_000));
    }

    #[tokio::test]
    async fn new_command_rotates_active_channel_session() {
        let gateway = test_gateway(vec!["owner-1".to_owned()]);
        let inbound = ChannelInbound {
            message_id: "321".to_owned(),
            channel_id: "123".to_owned(),
            guild_id: Some("10".to_owned()),
            is_dm: false,
            user_id: "owner-1".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            content: "hello".to_owned(),
        };
        let first = crate::gateway::router::route_inbound(
            GatewayChannelKind::Discord,
            inbound.clone(),
            &RoutingConfig::default(),
            &gateway.session_store,
        )
        .await
        .expect("routing should succeed")
        .expect("first inbound should route");
        assert_eq!(first.session_key, "agent:default:discord:123:session:1");

        let (reply_tx, reply_rx) = oneshot::channel();
        gateway
            .execute_command(ChannelCommand {
                kind: ChannelCommandKind::NewSession,
                source: GatewayChannelKind::Discord,
                channel_id: "123".to_owned(),
                guild_id: Some("10".to_owned()),
                user_id: "owner-1".to_owned(),
                is_dm: false,
                reply_tx,
            })
            .await;
        let response = reply_rx.await.expect("command reply should be sent");
        assert!(
            response
                .message()
                .contains("agent:default:discord:123:session:2")
        );

        let second = crate::gateway::router::route_inbound(
            GatewayChannelKind::Discord,
            inbound,
            &RoutingConfig::default(),
            &gateway.session_store,
        )
        .await
        .expect("routing should succeed")
        .expect("second inbound should route");
        assert_eq!(second.session_key, "agent:default:discord:123:session:2");
    }

    #[tokio::test]
    async fn slash_commands_are_owner_only() {
        let gateway = test_gateway(vec!["owner-1".to_owned()]);
        let (reply_tx, reply_rx) = oneshot::channel();

        gateway
            .execute_command(ChannelCommand {
                kind: ChannelCommandKind::Status,
                source: GatewayChannelKind::Discord,
                channel_id: "123".to_owned(),
                guild_id: Some("10".to_owned()),
                user_id: "user-2".to_owned(),
                is_dm: false,
                reply_tx,
            })
            .await;

        let response = reply_rx.await.expect("command reply should be sent");
        assert_eq!(
            response.message(),
            "That command is only available to configured owners."
        );
    }

    #[tokio::test]
    async fn status_command_includes_scope_and_session() {
        let gateway = test_gateway(vec!["owner-1".to_owned()]);
        let (reply_tx, reply_rx) = oneshot::channel();

        gateway
            .execute_command(ChannelCommand {
                kind: ChannelCommandKind::Status,
                source: GatewayChannelKind::Discord,
                channel_id: "123".to_owned(),
                guild_id: Some("10".to_owned()),
                user_id: "owner-1".to_owned(),
                is_dm: false,
                reply_tx,
            })
            .await;

        let response = reply_rx.await.expect("command reply should be sent");
        let message = response.message();
        assert!(message.contains("Scope: `agent:default:discord:123`"));
        assert!(message.contains("Session: `agent:default:discord:123:session:1`"));
    }
}
