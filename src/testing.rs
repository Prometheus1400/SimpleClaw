use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use color_eyre::eyre::WrapErr;
use tokio::sync::Mutex;

use crate::channels::{Channel, ChannelInbound, InboundMessage};
use crate::config::{AgentEntryConfig, GatewayChannelKind, GlobalConfig, LoadedConfig};
use crate::error::FrameworkError;
use crate::memory::{DynMemory, MemoryStore};
use crate::paths::AppPaths;
use crate::providers::{Message, Provider, ProviderFactory, ProviderResponse, ToolDefinition};
use crate::run::composition::{
    ChannelFactory, MemoryFactory, ProviderFactoryBuilder, RuntimeDependencies,
    assemble_runtime_state,
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
    /// Number of typing notifications emitted by the gateway.
    pub typing_events: usize,
    /// Memory session id used for message persistence.
    pub memory_session_id: String,
    /// Ephemeral paths that remain valid until this result is dropped.
    pub ephemeral_paths: EphemeralPaths,
}

/// Run one end-to-end gateway turn using a mock provider and ephemeral sqlite files.
pub async fn run_single_gateway_roundtrip(
    config: TestHarnessConfig,
) -> color_eyre::Result<TestTurnResult> {
    let ephemeral_paths = create_ephemeral_paths().wrap_err("failed to create ephemeral paths")?;

    let provider = Arc::new(StaticMockProvider::new(config.mock_reply.clone()));
    let channel = Arc::new(CaptureChannel::new());
    let deps = RuntimeDependencies {
        provider_factory_builder: Arc::new(StaticProviderFactory {
            provider: provider.clone(),
        }),
        memory_factory: Arc::new(EphemeralMemoryFactory {
            short_term_path: ephemeral_paths.short_term_db_path.clone(),
            long_term_path: ephemeral_paths.long_term_db_path.clone(),
        }),
        channel_factory: Arc::new(StaticChannelFactory {
            channel: channel.clone(),
        }),
        ..RuntimeDependencies::default()
    };

    let workspace_dir = ephemeral_paths.workspace_dir.clone();
    fs::create_dir_all(&workspace_dir).wrap_err("failed to create test workspace")?;

    let mut global = GlobalConfig::default();
    global.execution.defaults.memory_preinject.enabled = false;
    global.execution.defaults.max_steps = config.max_steps;
    global.agents.default = config.agent_id.clone();
    global.agents.list = vec![AgentEntryConfig {
        id: config.agent_id.clone(),
        name: config.agent_name.clone(),
        workspace: workspace_dir.clone(),
        config: crate::config::AgentInnerConfig::default(),
    }];
    global.gateway.channels = HashMap::from([(
        GatewayChannelKind::Discord,
        crate::config::ChannelConfig::default(),
    )]);
    let loaded = LoadedConfig { global };

    let app_base_dir = ephemeral_paths.root_dir.join("app");
    fs::create_dir_all(&app_base_dir).wrap_err("failed to create app base directory")?;
    let app_paths = AppPaths {
        base_dir: app_base_dir.clone(),
        config_path: app_base_dir.join("config.yaml"),
        secrets_path: app_base_dir.join("secrets.yaml"),
        db_path: ephemeral_paths.short_term_db_path.clone(),
        long_term_db_path: ephemeral_paths.long_term_db_path.clone(),
        fastembed_cache_dir: ephemeral_paths.fastembed_cache_dir.clone(),
        logs_dir: app_base_dir.join("logs"),
        log_path: app_base_dir.join("logs/service.log"),
        run_dir: app_base_dir.join("run"),
        pid_path: app_base_dir.join("run/service.pid"),
    };

    let (state, _inbound_rx) = assemble_runtime_state(&loaded, &app_paths, &deps)
        .await
        .wrap_err("failed to assemble runtime state for integration harness")?;

    let inbound = InboundMessage {
        trace_id: crate::telemetry::next_trace_id(),
        source_channel: GatewayChannelKind::Discord,
        target_agent_id: config.agent_id.clone(),
        session_key: format!("agent:{}:discord:{}", config.agent_id, config.channel_id),
        channel_id: config.channel_id.clone(),
        guild_id: None,
        is_dm: false,
        user_id: config.user_id.clone(),
        username: config.username.clone(),
        mentioned_bot: false,
        invoke: true,
        content: config.inbound_content.clone(),
    };
    let memory_session_id = inbound.session_key.clone();
    handle_inbound_once(&state, inbound)
        .await
        .wrap_err("failed to process one inbound message")?;

    Ok(TestTurnResult {
        outbound_messages: channel.outbound_messages().await,
        provider_call_count: provider.call_count(),
        typing_events: channel.typing_events(),
        memory_session_id,
        ephemeral_paths,
    })
}

struct StaticMockProvider {
    reply: String,
    call_count: AtomicUsize,
}

impl StaticMockProvider {
    fn new(reply: String) -> Self {
        Self {
            reply,
            call_count: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for StaticMockProvider {
    async fn generate(
        &self,
        _system_prompt: &str,
        _history: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderResponse {
            output_text: Some(self.reply.clone()),
            tool_calls: Vec::new(),
        })
    }
}

struct CaptureChannel {
    outbound: Mutex<Vec<TestOutboundMessage>>,
    typing_events: AtomicUsize,
    listen_tx: tokio::sync::mpsc::Sender<ChannelInbound>,
    listen_rx: Mutex<tokio::sync::mpsc::Receiver<ChannelInbound>>,
}

impl CaptureChannel {
    fn new() -> Self {
        let (listen_tx, listen_rx) = tokio::sync::mpsc::channel(1);
        Self {
            outbound: Mutex::new(Vec::new()),
            typing_events: AtomicUsize::new(0),
            listen_tx,
            listen_rx: Mutex::new(listen_rx),
        }
    }

    async fn outbound_messages(&self) -> Vec<TestOutboundMessage> {
        self.outbound.lock().await.clone()
    }

    fn typing_events(&self) -> usize {
        self.typing_events.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Channel for CaptureChannel {
    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError> {
        let mut outbound = self.outbound.lock().await;
        outbound.push(TestOutboundMessage {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
        });
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
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl ProviderFactoryBuilder for StaticProviderFactory {
    async fn create_provider_factory(
        &self,
        _loaded: &LoadedConfig,
    ) -> color_eyre::Result<ProviderFactory> {
        Ok(ProviderFactory::from_parts(HashMap::from([(
            "default".to_owned(),
            (
                Box::new(ForwardProvider {
                    inner: Arc::clone(&self.provider),
                }) as Box<dyn Provider>,
                true,
            ),
        )])))
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
    short_term_path: PathBuf,
    long_term_path: PathBuf,
}

#[async_trait]
impl MemoryFactory for EphemeralMemoryFactory {
    async fn create_memory(
        &self,
        _agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<DynMemory> {
        if let Some(parent) = self.short_term_path.parent() {
            fs::create_dir_all(parent).wrap_err("failed to create short-term db directory")?;
        }
        if let Some(parent) = self.long_term_path.parent() {
            fs::create_dir_all(parent).wrap_err("failed to create long-term db directory")?;
        }
        let _ = &loaded.global.embedding;
        MemoryStore::new_without_embedder(
            &self.short_term_path,
            &self.long_term_path,
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
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, self.channel.clone());
        Ok(channels)
    }
}

fn create_ephemeral_paths() -> color_eyre::Result<EphemeralPaths> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .wrap_err("clock drift while building integration temp dir")?
        .as_nanos();
    let root_dir = std::env::temp_dir().join(format!("simpleclaw_integration_{nanos}"));
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
        fastembed_cache_dir,
    })
}
