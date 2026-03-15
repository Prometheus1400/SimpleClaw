use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use color_eyre::eyre::WrapErr;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

use crate::channels::{Channel, ChannelInbound, InboundMessage};
use crate::config::{
    AgentEntryConfig, AgentInnerConfig, GatewayChannelKind, GlobalConfig, InboundPolicyConfig,
    LoadedConfig,
};
use crate::error::FrameworkError;
use crate::memory::{DynMemory, MemoryStore};
use crate::paths::AppPaths;
use crate::providers::{Message, Provider, ProviderFactory, ProviderResponse, ToolDefinition};
use crate::run::composition::{
    ChannelFactory, MemoryFactory, ProviderFactoryBuilder, RuntimeDependencies,
    assemble_runtime_state, start_runtime_services,
};
use crate::run::handle_inbound_once;

/// Configuration for a single end-to-end roundtrip test run.
#[derive(Debug, Clone)]
pub struct TestHarnessConfig {
    /// Target agent id used for routing and session key composition.
    pub agent_id: String,
    /// User-visible agent name stored in generated config.
    pub agent_name: String,
    /// Inbound text delivered to the gateway.
    pub inbound_content: String,
    /// Mock provider output returned as the assistant reply.
    pub mock_reply: String,
    /// Channel/session id for the inbound message.
    pub channel_id: String,
    /// Logical user id for the inbound message.
    pub user_id: String,
    /// Display username for the inbound message.
    pub username: String,
    /// Max steps used for runtime creation.
    pub max_steps: u32,
    /// Optional scripted tool call emitted on the first provider turn.
    pub scripted_tool_call: Option<ScriptedToolCall>,
    /// Optional final reply emitted after scripted tool call completion.
    pub scripted_final_reply: Option<String>,
    /// Optional override for tools.exec.timeout_seconds.
    pub exec_timeout_seconds: Option<u64>,
    /// Whether to push the inbound event through the real gateway listener/queue path.
    pub route_via_gateway_listener: bool,
    /// Whether the source inbound should be treated as a DM.
    pub is_dm: bool,
    /// Whether the source inbound mentions the bot.
    pub mentioned_bot: bool,
    /// Whether routing should require a mention before invoking.
    pub require_mentions: bool,
    /// Optional allowlist used by gateway routing.
    pub allow_from: Option<Vec<String>>,
    /// Whether a listener-path test expects the inbound to be dropped before routing.
    pub expect_listener_drop: bool,
    /// Additional user-visible inbound contents to route after the initial turn.
    pub follow_up_inbound_contents: Vec<String>,
    /// Additional inbound messages to process after the initial turn.
    pub additional_inbounds_to_process: usize,
    /// Timeout used when waiting for additional inbound messages.
    pub additional_inbound_timeout_ms: u64,
    /// Optional explicit agent list for multi-agent scenarios.
    pub agents: Vec<TestAgentConfig>,
}

/// Agent/runtime configuration used by multi-agent integration tests.
#[derive(Debug, Clone)]
pub struct TestAgentConfig {
    /// Agent id used in routing and directory lookup.
    pub id: String,
    /// User-visible name stored in generated config.
    pub name: String,
    /// Provider key registered in the mock provider factory.
    pub provider_key: String,
    /// Concrete agent config used for runtime assembly.
    pub agent_config: AgentInnerConfig,
    /// Scripted provider steps returned for this agent.
    pub script: Vec<ProviderScriptStep>,
}

/// Script step returned by a mocked provider during integration tests.
#[derive(Debug, Clone)]
pub enum ProviderScriptStep {
    /// Emit a final assistant reply.
    Reply(String),
    /// Emit one tool call for the current provider turn.
    ToolCall(ScriptedToolCall),
    /// Return a provider error for the current turn.
    Error(String),
}

impl TestAgentConfig {
    /// Build a test agent with default runtime configuration and a scripted provider.
    pub fn new(id: &str, name: &str, provider_key: &str, script: Vec<ProviderScriptStep>) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            provider_key: provider_key.to_owned(),
            agent_config: AgentInnerConfig::default(),
            script,
        }
    }

    /// Override the agent's exec tool settings.
    pub fn with_exec_tool(
        mut self,
        timeout_seconds: Option<u64>,
        allow_background: bool,
        sandbox_enabled: bool,
    ) -> Self {
        self.agent_config.tools.exec = Some(crate::config::ExecToolConfig {
            timeout_seconds,
            allow_background,
            sandbox: crate::config::ToolSandboxConfig {
                enabled: sandbox_enabled,
                ..Default::default()
            },
            ..Default::default()
        });
        self
    }

    /// Enable the list tool with optional WASM sandboxing for this test agent.
    pub fn with_list_tool(mut self, sandbox_enabled: bool) -> Self {
        self.agent_config.tools.list = Some(crate::config::ListToolConfig {
            sandbox: crate::config::ToolSandboxConfig {
                enabled: sandbox_enabled,
                ..Default::default()
            },
            ..Default::default()
        });
        self
    }

    /// Enable the grep tool with optional WASM sandboxing for this test agent.
    pub fn with_grep_tool(mut self, sandbox_enabled: bool) -> Self {
        self.agent_config.tools.grep = Some(crate::config::GrepToolConfig {
            sandbox: crate::config::ToolSandboxConfig {
                enabled: sandbox_enabled,
                ..Default::default()
            },
            ..Default::default()
        });
        self
    }

    /// Enable the glob tool with optional WASM sandboxing for this test agent.
    pub fn with_glob_tool(mut self, sandbox_enabled: bool) -> Self {
        self.agent_config.tools.glob = Some(crate::config::GlobToolConfig {
            sandbox: crate::config::ToolSandboxConfig {
                enabled: sandbox_enabled,
                ..Default::default()
            },
            ..Default::default()
        });
        self
    }

    /// Restrict summon targets for this test agent.
    pub fn with_summon_allowed(mut self, allowed: Vec<String>) -> Self {
        self.agent_config.tools.summon = Some(crate::config::SummonToolConfig {
            allowed,
            ..Default::default()
        });
        self
    }

    /// Set the task worker max-steps override for this test agent.
    pub fn with_task_worker_max_steps(mut self, worker_max_steps: Option<u32>) -> Self {
        self.agent_config.tools.task = Some(crate::config::TaskToolConfig {
            worker_max_steps,
            ..Default::default()
        });
        self
    }

    /// Override the provider key used for this agent.
    pub fn with_provider_key(mut self, provider_key: &str) -> Self {
        self.provider_key = provider_key.to_owned();
        self.agent_config.provider = Some(provider_key.to_owned());
        self
    }
}

/// Tool call emitted by the scripted test provider on its first turn.
#[derive(Debug, Clone)]
pub struct ScriptedToolCall {
    /// Optional provider tool-call identifier.
    pub id: Option<String>,
    /// Tool name to invoke (for example, `exec`).
    pub name: String,
    /// JSON-encoded tool arguments.
    pub args_json: String,
}

impl Default for TestHarnessConfig {
    fn default() -> Self {
        Self {
            agent_id: "default".to_owned(),
            agent_name: "Default".to_owned(),
            inbound_content: "hello from integration test".to_owned(),
            mock_reply: "mock reply".to_owned(),
            channel_id: "integration-channel".to_owned(),
            user_id: "integration-user".to_owned(),
            username: "integration-user".to_owned(),
            max_steps: 4,
            scripted_tool_call: None,
            scripted_final_reply: None,
            exec_timeout_seconds: None,
            route_via_gateway_listener: false,
            is_dm: false,
            mentioned_bot: false,
            require_mentions: false,
            allow_from: None,
            expect_listener_drop: false,
            follow_up_inbound_contents: Vec::new(),
            additional_inbounds_to_process: 0,
            additional_inbound_timeout_ms: 2_000,
            agents: Vec::new(),
        }
    }
}

/// Outbound message captured from the gateway channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutboundMessage {
    /// Session id passed to the channel send call.
    pub channel_id: String,
    /// Message content emitted by runtime processing.
    pub content: String,
}

/// Ephemeral filesystem paths used by the integration harness.
#[derive(Debug)]
pub struct EphemeralPaths {
    /// Temporary root directory for all test artifacts.
    pub root_dir: PathBuf,
    /// Workspace directory used by the configured test agent.
    pub workspace_dir: PathBuf,
    /// Short-term SQLite database path.
    pub short_term_db_path: PathBuf,
    /// Long-term SQLite database path.
    pub long_term_db_path: PathBuf,
    /// Directory containing all sqlite artifacts for the harness run.
    pub db_dir: PathBuf,
    /// Fastembed cache directory used for memory initialization.
    pub fastembed_cache_dir: PathBuf,
}

impl Drop for EphemeralPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root_dir);
    }
}

/// Result for one gateway roundtrip harness execution.
#[derive(Debug)]
pub struct TestTurnResult {
    /// Outbound messages captured by the test channel.
    pub outbound_messages: Vec<TestOutboundMessage>,
    /// Number of mock provider generate calls.
    pub provider_call_count: usize,
    /// Per-provider generate call counts.
    pub provider_call_counts: HashMap<String, usize>,
    /// Number of typing notifications emitted by the gateway.
    pub typing_events: usize,
    /// Memory session id used for message persistence.
    pub memory_session_id: String,
    /// Whether the listener path routed an inbound into runtime execution.
    pub listener_routed: bool,
    /// Ephemeral paths that remain valid until this result is dropped.
    pub ephemeral_paths: EphemeralPaths,
    /// Whether provider observed at least one tool result in history.
    pub observed_tool_result: bool,
    /// Last tool result payload observed by the provider, if any.
    pub observed_tool_response: Option<Value>,
    /// All tool result payloads observed by the provider in call order.
    pub observed_tool_responses: Vec<Value>,
}

/// Run one end-to-end gateway turn using a mock provider and ephemeral sqlite files.
pub async fn run_single_gateway_roundtrip(
    config: TestHarnessConfig,
) -> color_eyre::Result<TestTurnResult> {
    let ephemeral_paths = create_ephemeral_paths().wrap_err("failed to create ephemeral paths")?;

    let agent_specs = effective_agents(&config);
    let primary_provider_key = agent_specs
        .iter()
        .find(|agent| agent.id == config.agent_id)
        .map(|agent| agent.provider_key.clone())
        .unwrap_or_else(|| "default".to_owned());
    let providers = build_scripted_providers(&agent_specs);
    let channel = Arc::new(CaptureChannel::new());
    let deps = RuntimeDependencies {
        provider_factory_builder: Arc::new(StaticProviderFactory {
            providers: providers.clone(),
        }),
        memory_factory: Arc::new(EphemeralMemoryFactory {
            db_dir: ephemeral_paths.db_dir.clone(),
            primary_agent_id: config.agent_id.clone(),
            primary_short_term_path: ephemeral_paths.short_term_db_path.clone(),
            primary_long_term_path: ephemeral_paths.long_term_db_path.clone(),
        }),
        channel_factory: Arc::new(StaticChannelFactory {
            channel: channel.clone(),
        }),
        ..RuntimeDependencies::default()
    };

    let workspace_dir = ephemeral_paths.workspace_dir.clone();
    fs::create_dir_all(&workspace_dir).wrap_err("failed to create test workspace")?;

    let mut global = GlobalConfig::default();
    global.execution.defaults.memory_recall.enabled = false;
    global.execution.defaults.max_steps = config.max_steps;
    global.execution.owner_ids = vec![config.user_id.clone()];
    global.agents.default = config.agent_id.clone();
    global.gateway.routing.defaults = InboundPolicyConfig {
        agent: Some(config.agent_id.clone()),
        allow_from: config.allow_from.clone(),
        require_mentions: Some(config.require_mentions),
    };
    for agent in &agent_specs {
        fs::create_dir_all(workspace_dir.join("personas").join(&agent.id))
            .wrap_err("failed to create test agent persona")?;
        fs::create_dir_all(workspace_dir.join(&agent.id))
            .wrap_err("failed to create test agent workspace")?;
    }
    global.agents.list = agent_specs
        .iter()
        .map(|agent| AgentEntryConfig {
            id: agent.id.clone(),
            name: agent.name.clone(),
            persona: workspace_dir.join("personas").join(&agent.id),
            workspace: workspace_dir.join(&agent.id),
            config: agent.agent_config.clone(),
        })
        .collect();
    global.gateway.channels = HashMap::from([(
        GatewayChannelKind::Discord,
        crate::config::ChannelConfig::default(),
    )]);
    let loaded = LoadedConfig { global };

    let app_base_dir = ephemeral_paths.root_dir.join("app");
    fs::create_dir_all(&app_base_dir).wrap_err("failed to create app base directory")?;
    let app_paths = AppPaths {
        base_dir: app_base_dir.clone(),
        bin_dir: app_base_dir.join("bin"),
        models_dir: app_base_dir.join("models"),
        venvs_dir: app_base_dir.join("venvs"),
        config_path: app_base_dir.join("config.yaml"),
        secrets_path: app_base_dir.join("secrets.yaml"),
        db_path: ephemeral_paths.short_term_db_path.clone(),
        long_term_db_path: ephemeral_paths.long_term_db_path.clone(),
        cron_db_path: app_base_dir.join("db/cron.db"),
        fastembed_cache_dir: ephemeral_paths.fastembed_cache_dir.clone(),
        logs_dir: app_base_dir.join("logs"),
        log_path: app_base_dir.join("logs/service.log"),
        run_dir: app_base_dir.join("run"),
        pid_path: app_base_dir.join("run/service.pid"),
    };

    let (state, mut inbound_rx) = assemble_runtime_state(&loaded, &app_paths, &deps)
        .await
        .wrap_err("failed to assemble runtime state for integration harness")?;
    let _runtime_services = config
        .route_via_gateway_listener
        .then(|| start_runtime_services(&state));

    let (memory_session_id, listener_routed) = if config.route_via_gateway_listener {
        run_via_gateway_listener(&state, &mut inbound_rx, &channel, &config).await?
    } else {
        let inbound = InboundMessage {
            trace_id: crate::telemetry::next_trace_id(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: config.agent_id.clone(),
            session_key: format!("agent:{}:discord:{}", config.agent_id, config.channel_id),
            source_message_id: Some("test-message-1".to_owned()),
            channel_id: config.channel_id.clone(),
            guild_id: None,
            is_dm: config.is_dm,
            user_id: config.user_id.clone(),
            username: config.username.clone(),
            mentioned_bot: config.mentioned_bot,
            invoke: true,
            content: config.inbound_content.clone(),
            kind: crate::channels::InboundMessageKind::Text,
        };
        let memory_session_id = inbound.session_key.clone();
        handle_inbound_once(&state, inbound)
            .await
            .wrap_err("failed to process one inbound message")?;
        (memory_session_id, true)
    };

    for content in &config.follow_up_inbound_contents {
        let inbound = receive_listener_inbound(&mut inbound_rx, &channel, &config, content)
            .await
            .wrap_err("failed to route follow-up inbound through gateway listener")?;
        handle_inbound_once(&state, inbound)
            .await
            .wrap_err("failed to process routed follow-up inbound message")?;
    }

    for _ in 0..config.additional_inbounds_to_process {
        let inbound = timeout(
            Duration::from_millis(config.additional_inbound_timeout_ms),
            inbound_rx.recv(),
        )
        .await
        .wrap_err("timed out waiting for follow-up inbound")?
        .ok_or_else(|| color_eyre::eyre::eyre!("gateway inbound queue closed unexpectedly"))?;
        handle_inbound_once(&state, inbound)
            .await
            .wrap_err("failed to process follow-up inbound message")?;
    }

    let provider_call_counts = providers
        .iter()
        .map(|(key, provider)| (key.clone(), provider.call_count()))
        .collect::<HashMap<_, _>>();
    let primary_provider = providers
        .get(&primary_provider_key)
        .expect("primary provider should exist");

    Ok(TestTurnResult {
        outbound_messages: channel.outbound_messages().await,
        provider_call_count: provider_call_counts.values().copied().sum(),
        provider_call_counts,
        typing_events: channel.typing_events(),
        memory_session_id,
        listener_routed,
        ephemeral_paths,
        observed_tool_result: primary_provider.saw_tool_result(),
        observed_tool_response: primary_provider.observed_tool_response(),
        observed_tool_responses: primary_provider.observed_tool_responses(),
    })
}

async fn run_via_gateway_listener(
    state: &crate::run::composition::RuntimeState,
    inbound_rx: &mut tokio::sync::mpsc::Receiver<InboundMessage>,
    channel: &Arc<CaptureChannel>,
    config: &TestHarnessConfig,
) -> color_eyre::Result<(String, bool)> {
    let inbound =
        receive_listener_inbound(inbound_rx, channel, config, &config.inbound_content).await;
    let inbound = match inbound {
        Ok(inbound) => inbound,
        Err(_) if config.expect_listener_drop => return Ok((String::new(), false)),
        Err(err) => return Err(err),
    };
    let memory_session_id = inbound.session_key.clone();
    handle_inbound_once(state, inbound)
        .await
        .wrap_err("failed to process routed inbound message")?;
    Ok((memory_session_id, true))
}

async fn receive_listener_inbound(
    inbound_rx: &mut tokio::sync::mpsc::Receiver<InboundMessage>,
    channel: &Arc<CaptureChannel>,
    config: &TestHarnessConfig,
    content: &str,
) -> color_eyre::Result<InboundMessage> {
    channel
        .push_inbound(ChannelInbound {
            message_id: "test-message-1".to_owned(),
            channel_id: config.channel_id.clone(),
            guild_id: None,
            is_dm: config.is_dm,
            user_id: config.user_id.clone(),
            username: config.username.clone(),
            mentioned_bot: config.mentioned_bot,
            content: content.to_owned(),
            kind: crate::channels::InboundMessageKind::Text,
        })
        .await
        .wrap_err("failed to enqueue test inbound for gateway listener")?;

    match timeout(Duration::from_secs(1), inbound_rx.recv()).await {
        Ok(Some(inbound)) => Ok(inbound),
        Ok(None) => Err(color_eyre::eyre::eyre!(
            "gateway inbound queue closed unexpectedly"
        )),
        Err(_) => Err(color_eyre::eyre::eyre!(
            "timed out waiting for gateway listener to emit inbound"
        )),
    }
}

struct StaticMockProvider {
    script: Vec<ProviderScriptStep>,
    call_count: AtomicUsize,
    saw_tool_result: AtomicBool,
    observed_tool_response: StdMutex<Option<Value>>,
    observed_tool_responses: StdMutex<Vec<Value>>,
}

impl StaticMockProvider {
    fn new(script: Vec<ProviderScriptStep>) -> Self {
        Self {
            script,
            call_count: AtomicUsize::new(0),
            saw_tool_result: AtomicBool::new(false),
            observed_tool_response: StdMutex::new(None),
            observed_tool_responses: StdMutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    fn saw_tool_result(&self) -> bool {
        self.saw_tool_result.load(Ordering::SeqCst)
    }

    fn observed_tool_response(&self) -> Option<Value> {
        self.observed_tool_response
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn observed_tool_responses(&self) -> Vec<Value> {
        self.observed_tool_responses
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Provider for StaticMockProvider {
    async fn generate(
        &self,
        _system_prompt: &str,
        history: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        let call_index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(message) = history.last().filter(|m| !m.tool_results.is_empty()) {
            self.saw_tool_result.store(true, Ordering::SeqCst);
            if let Some(result) = message.tool_results.first()
                && let Ok(mut slot) = self.observed_tool_response.lock()
            {
                *slot = Some(result.response.clone());
            }
            if let Some(result) = message.tool_results.first()
                && let Ok(mut slot) = self.observed_tool_responses.lock()
            {
                slot.push(result.response.clone());
            }
        }

        let step = self.script.get(call_index).cloned().unwrap_or_else(|| {
            self.script
                .last()
                .cloned()
                .unwrap_or_else(|| ProviderScriptStep::Reply(String::new()))
        });
        match step {
            ProviderScriptStep::Reply(reply) => Ok(ProviderResponse {
                output_text: Some(reply),
                tool_calls: Vec::new(),
            }),
            ProviderScriptStep::ToolCall(tool_call) => Ok(ProviderResponse {
                output_text: None,
                tool_calls: vec![crate::providers::ToolCall {
                    id: tool_call.id,
                    name: tool_call.name,
                    args_json: tool_call.args_json,
                }],
            }),
            ProviderScriptStep::Error(message) => Err(FrameworkError::Tool(message)),
        }
    }
}

struct CaptureChannel {
    outbound: Mutex<Vec<TestOutboundMessage>>,
    outbound_ids: Mutex<Vec<String>>,
    typing_events: AtomicUsize,
    listen_tx: tokio::sync::mpsc::Sender<ChannelInbound>,
    listen_rx: Mutex<tokio::sync::mpsc::Receiver<ChannelInbound>>,
    next_message_id: AtomicUsize,
}

impl CaptureChannel {
    fn new() -> Self {
        let (listen_tx, listen_rx) = tokio::sync::mpsc::channel(1);
        Self {
            outbound: Mutex::new(Vec::new()),
            outbound_ids: Mutex::new(Vec::new()),
            typing_events: AtomicUsize::new(0),
            listen_tx,
            listen_rx: Mutex::new(listen_rx),
            next_message_id: AtomicUsize::new(0),
        }
    }

    async fn outbound_messages(&self) -> Vec<TestOutboundMessage> {
        self.outbound.lock().await.clone()
    }

    fn typing_events(&self) -> usize {
        self.typing_events.load(Ordering::SeqCst)
    }

    async fn push_inbound(&self, inbound: ChannelInbound) -> Result<(), FrameworkError> {
        self.listen_tx
            .send(inbound)
            .await
            .map_err(|err| FrameworkError::Config(format!("test inbound enqueue failed: {err}")))
    }
}

#[async_trait]
impl Channel for CaptureChannel {
    fn supports_message_editing(&self) -> bool {
        true
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError> {
        let mut outbound = self.outbound.lock().await;
        outbound.push(TestOutboundMessage {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
        });
        self.outbound_ids.lock().await.push(String::new());
        Ok(())
    }

    async fn send_message_with_id(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Option<String>, FrameworkError> {
        let message_id = format!(
            "test-msg-{}",
            self.next_message_id.fetch_add(1, Ordering::SeqCst)
        );
        let mut outbound = self.outbound.lock().await;
        outbound.push(TestOutboundMessage {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
        });
        self.outbound_ids.lock().await.push(message_id.clone());
        Ok(Some(message_id))
    }

    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), FrameworkError> {
        let mut outbound = self.outbound.lock().await;
        let outbound_ids = self.outbound_ids.lock().await;
        if let Some((index, _)) = outbound_ids
            .iter()
            .enumerate()
            .find(|(_, candidate)| candidate.as_str() == message_id)
        {
            outbound[index] = TestOutboundMessage {
                channel_id: channel_id.to_owned(),
                content: content.to_owned(),
            };
            return Ok(());
        }

        outbound.push(TestOutboundMessage {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
        });
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
        self.typing_events.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
        let _keep_sender_alive = &self.listen_tx;
        let mut rx = self.listen_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| FrameworkError::Config("test inbound channel closed".to_owned()))
    }
}

struct StaticProviderFactory {
    providers: HashMap<String, Arc<StaticMockProvider>>,
}

#[async_trait]
impl ProviderFactoryBuilder for StaticProviderFactory {
    async fn create_provider_factory(
        &self,
        _loaded: &LoadedConfig,
    ) -> color_eyre::Result<ProviderFactory> {
        let parts = self
            .providers
            .iter()
            .map(|(key, provider)| {
                (
                    key.clone(),
                    (
                        Box::new(ForwardProvider {
                            inner: provider.clone() as Arc<dyn Provider>,
                        }) as Box<dyn Provider>,
                        true,
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        Ok(ProviderFactory::from_parts(parts))
    }
}

struct ForwardProvider {
    inner: Arc<dyn Provider>,
}

#[async_trait]
impl Provider for ForwardProvider {
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        self.inner.generate(system_prompt, history, tools).await
    }
}

struct EphemeralMemoryFactory {
    db_dir: PathBuf,
    primary_agent_id: String,
    primary_short_term_path: PathBuf,
    primary_long_term_path: PathBuf,
}

#[async_trait]
impl MemoryFactory for EphemeralMemoryFactory {
    async fn create_memory(
        &self,
        agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<DynMemory> {
        let (short_term_path, long_term_path) = if agent.id == self.primary_agent_id {
            (
                self.primary_short_term_path.clone(),
                self.primary_long_term_path.clone(),
            )
        } else {
            (
                self.db_dir.join(format!("{}_short.db", agent.id)),
                self.db_dir.join(format!("{}_long.db", agent.id)),
            )
        };
        if let Some(parent) = short_term_path.parent() {
            fs::create_dir_all(parent).wrap_err("failed to create short-term db directory")?;
        }
        if let Some(parent) = long_term_path.parent() {
            fs::create_dir_all(parent).wrap_err("failed to create long-term db directory")?;
        }
        let _ = &loaded.global.embedding;
        MemoryStore::new_without_embedder(
            &short_term_path,
            &long_term_path,
            &loaded.global.database,
        )
        .await
        .map(|memory| Arc::new(memory) as DynMemory)
        .map_err(color_eyre::Report::from)
    }
}

struct StaticChannelFactory {
    channel: Arc<CaptureChannel>,
}

#[async_trait]
impl ChannelFactory for StaticChannelFactory {
    async fn create_channels(
        &self,
        _loaded: &LoadedConfig,
        _approval_registry: Arc<crate::approval::ApprovalRegistry>,
        _transcriber: Option<Arc<dyn crate::audio::Transcriber>>,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, self.channel.clone());
        Ok(channels)
    }
}

fn create_ephemeral_paths() -> color_eyre::Result<EphemeralPaths> {
    static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .wrap_err("clock drift while building integration temp dir")?
        .as_nanos();
    let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root_dir = std::env::temp_dir().join(format!("simpleclaw_integration_{nanos}_{nonce}"));
    let workspace_dir = root_dir.join("workspace");
    let db_dir = root_dir.join("db");
    let short_term_db_path = db_dir.join("short.db");
    let long_term_db_path = db_dir.join("long.db");
    let fastembed_cache_dir = root_dir.join(".fastembed_cache");
    fs::create_dir_all(&workspace_dir).wrap_err("failed to create integration workspace")?;
    fs::create_dir_all(&db_dir).wrap_err("failed to create integration db dir")?;
    fs::create_dir_all(&fastembed_cache_dir)
        .wrap_err("failed to create integration fastembed cache dir")?;

    Ok(EphemeralPaths {
        root_dir,
        workspace_dir,
        short_term_db_path,
        long_term_db_path,
        db_dir,
        fastembed_cache_dir,
    })
}

fn effective_agents(config: &TestHarnessConfig) -> Vec<TestAgentConfig> {
    if !config.agents.is_empty() {
        return config.agents.clone();
    }

    let mut agent_config = AgentInnerConfig::default();
    if let Some(timeout_seconds) = config.exec_timeout_seconds {
        agent_config.tools.exec = Some(crate::config::ExecToolConfig {
            timeout_seconds: Some(timeout_seconds),
            ..Default::default()
        });
    }

    let script = if let Some(tool_call) = config.scripted_tool_call.clone() {
        vec![
            ProviderScriptStep::ToolCall(tool_call),
            ProviderScriptStep::Reply(
                config
                    .scripted_final_reply
                    .clone()
                    .unwrap_or_else(|| config.mock_reply.clone()),
            ),
        ]
    } else {
        vec![ProviderScriptStep::Reply(config.mock_reply.clone())]
    };

    vec![TestAgentConfig {
        id: config.agent_id.clone(),
        name: config.agent_name.clone(),
        provider_key: "default".to_owned(),
        agent_config,
        script,
    }]
}

fn build_scripted_providers(
    agents: &[TestAgentConfig],
) -> HashMap<String, Arc<StaticMockProvider>> {
    agents
        .iter()
        .map(|agent| {
            (
                agent.provider_key.clone(),
                Arc::new(StaticMockProvider::new(agent.script.clone())),
            )
        })
        .collect()
}
