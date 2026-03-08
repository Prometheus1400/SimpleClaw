use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use color_eyre::eyre::WrapErr;
use tokio::sync::mpsc;
use tracing::info;

use crate::agent::{
    AgentRuntime, AgentRuntimeConfig, load_system_prompt_for_workspace,
};
use crate::channels::{Channel, DiscordChannel, InboundMessage};
use crate::cli::Cli;
use crate::config::{AgentEntryConfig, GatewayChannelKind, LoadedConfig};
use crate::gateway::Gateway;
use crate::memory::MemoryStore;
use crate::paths::AppPaths;
use crate::providers::ProviderFactory;
use crate::react::ReactLoop;
use crate::tools::skill::SkillFactory;
use crate::tools::{ProcessManager, default_factory};

#[async_trait]
pub(crate) trait ProviderFactoryBuilder: Send + Sync {
    async fn create_provider_factory(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<ProviderFactory>;
}

#[async_trait]
pub(crate) trait MemoryFactory: Send + Sync {
    async fn create_memory(
        &self,
        agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<MemoryStore>;
}

#[async_trait]
pub(crate) trait ChannelFactory: Send + Sync {
    async fn create_channels(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>>;
}

pub(crate) struct RuntimeDependencies {
    pub provider_factory_builder: Arc<dyn ProviderFactoryBuilder>,
    pub memory_factory: Arc<dyn MemoryFactory>,
    pub channel_factory: Arc<dyn ChannelFactory>,
}

impl Default for RuntimeDependencies {
    fn default() -> Self {
        Self {
            provider_factory_builder: Arc::new(DefaultProviderFactoryBuilder),
            memory_factory: Arc::new(DefaultMemoryFactory),
            channel_factory: Arc::new(DefaultChannelFactory),
        }
    }
}

struct DefaultProviderFactoryBuilder;

#[async_trait]
impl ProviderFactoryBuilder for DefaultProviderFactoryBuilder {
    async fn create_provider_factory(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<ProviderFactory> {
        ProviderFactory::from_config(&loaded.global.providers)
            .wrap_err("failed to initialize provider factory")
    }
}

struct DefaultMemoryFactory;

#[async_trait]
impl MemoryFactory for DefaultMemoryFactory {
    async fn create_memory(
        &self,
        agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<MemoryStore> {
        let (memory_dir, short_term_path, long_term_path) =
            agent_workspace_memory_paths(&agent.workspace);
        fs::create_dir_all(&memory_dir).wrap_err_with(|| {
            format!(
                "failed to create memory directory for agent '{}': {}",
                agent.id,
                memory_dir.display()
            )
        })?;
        MemoryStore::new(
            &short_term_path,
            &long_term_path,
            &loaded.global.database,
            &loaded.global.embedding,
        )
        .await
        .wrap_err_with(|| {
            format!(
                "failed to initialize sqlite memory store for agent '{}'",
                agent.id
            )
        })
    }
}

struct DefaultChannelFactory;

#[async_trait]
impl ChannelFactory for DefaultChannelFactory {
    async fn create_channels(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        for kind in &loaded.global.gateway.channels {
            let channel: Arc<dyn Channel> = match kind {
                GatewayChannelKind::Discord => Arc::new(
                    DiscordChannel::from_config(&loaded.global.discord)
                        .await
                        .wrap_err("failed to initialize discord channel")?,
                ),
            };
            channels.insert(*kind, channel);
        }
        Ok(channels)
    }
}

pub(crate) struct RuntimeState {
    pub gateway: Gateway,
    pub react_loop: Arc<ReactLoop>,
    pub runtimes: HashMap<String, AgentRuntime>,
    pub agent_configs: Arc<HashMap<String, AgentRuntimeConfig>>,
    pub memories: Arc<HashMap<String, MemoryStore>>,
    pub process_manager: Arc<ProcessManager>,
    pub completion_tx: mpsc::Sender<InboundMessage>,
    pub safe_error_reply: String,
}

pub(crate) async fn assemble_runtime_state(
    cli: &Cli,
    loaded: &LoadedConfig,
    app_paths: &AppPaths,
    deps: &RuntimeDependencies,
) -> color_eyre::Result<RuntimeState> {
    let provider_factory = deps
        .provider_factory_builder
        .create_provider_factory(loaded)
        .await?;

    let mut memories_map: HashMap<String, MemoryStore> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = deps.memory_factory.create_memory(agent, loaded).await?;
        memories_map.insert(agent.id.clone(), memory);
    }

    let mut agent_configs_map: HashMap<String, AgentRuntimeConfig> = HashMap::new();
    let mut skill_factory = SkillFactory::new(app_paths.base_dir.clone());

    for agent in &loaded.global.agents.list {
        let agent_config = agent.runtime.clone();
        let provider_key = agent_config
            .provider
            .as_deref()
            .unwrap_or(loaded.global.providers.default.as_str())
            .trim()
            .to_owned();
        if provider_key.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "agent '{}' has an empty provider key in config.yaml",
                agent.id
            ));
        }
        provider_factory.get(&provider_key).map_err(color_eyre::Report::from)?;

        let system_prompt = load_system_prompt_for_workspace(&agent.workspace).wrap_err_with(|| {
            format!(
                "failed to assemble layered system prompt for agent '{}'",
                agent.id
            )
        })?;
        let skill_tools = skill_factory
            .load_for_agent(&agent.id, &agent_config, &agent.workspace)
            .wrap_err_with(|| format!("failed to load skill tools for agent '{}'", agent.id))?;
        info!(
            agent_id = %agent.id,
            loaded_skill_tools = skill_tools.len(),
            status = "loaded",
            "agent skill tools loaded"
        );

        skill_factory.insert_agent_tools(agent.id.clone(), skill_tools);
        agent_configs_map.insert(
            agent.id.clone(),
            AgentRuntimeConfig {
                agent_id: agent.id.clone(),
                provider_key,
                runtime_config: loaded.global.runtime.clone(),
                agent_config,
                workspace_root: agent.workspace.clone(),
                app_base_dir: app_paths.base_dir.clone(),
                system_prompt,
                max_steps: cli.max_steps,
            },
        );
    }

    let tool_factory = default_factory();
    let react_loop = Arc::new(ReactLoop::new(
        provider_factory,
        tool_factory,
        skill_factory,
    ));

    let agent_configs = Arc::new(agent_configs_map);
    let memories = Arc::new(memories_map);
    let mut runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    for (agent_id, config) in agent_configs.iter() {
        runtimes.insert(agent_id.clone(), AgentRuntime::new(config.clone()));
    }

    let process_manager = Arc::new(ProcessManager::new());
    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::channel::<InboundMessage>(1_024);

    let channels = deps.channel_factory.create_channels(loaded).await?;
    let gateway = Gateway::new(
        channels,
        loaded.global.inbound.clone(),
        gateway_tx.clone(),
        gateway_rx,
    );

    Ok(RuntimeState {
        gateway,
        react_loop,
        runtimes,
        agent_configs,
        memories,
        process_manager,
        completion_tx: gateway_tx,
        safe_error_reply: loaded.global.runtime.safe_error_reply.clone(),
    })
}

pub(crate) fn agent_workspace_memory_paths(workspace: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let memory_dir = workspace.join(".simpleclaw").join("memory");
    let short_term_path = memory_dir.join("lraf.db");
    let long_term_path = memory_dir.join("lraf_long_term.db");
    (memory_dir, short_term_path, long_term_path)
}
