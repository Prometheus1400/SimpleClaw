use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use color_eyre::eyre::WrapErr;
use tracing::info;

use crate::agent::{
    AgentRuntime, AgentRuntimeConfig, build_tool_registry_for_agent,
    load_agent_config_for_workspace, load_system_prompt_for_workspace,
};
use crate::channels::{Channel, DiscordChannel, InboundMessage};
use crate::cli::Cli;
use crate::config::{AgentEntryConfig, GatewayChannelKind, LoadedConfig};
use crate::gateway::Gateway;
use crate::memory::MemoryStore;
use crate::paths::AppPaths;
use crate::providers::{Provider, ProviderMetadata, ProviderRegistry};

#[async_trait]
pub(crate) trait ProviderFactory: Send + Sync {
    async fn create_providers(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<String, ProviderHandle>>;
}

pub(crate) struct ProviderHandle {
    pub provider: Arc<dyn Provider>,
    pub metadata: ProviderMetadata,
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
    pub provider_factory: Arc<dyn ProviderFactory>,
    pub memory_factory: Arc<dyn MemoryFactory>,
    pub channel_factory: Arc<dyn ChannelFactory>,
}

impl Default for RuntimeDependencies {
    fn default() -> Self {
        Self {
            provider_factory: Arc::new(DefaultProviderFactory),
            memory_factory: Arc::new(DefaultMemoryFactory),
            channel_factory: Arc::new(DefaultChannelFactory),
        }
    }
}

struct DefaultProviderFactory;

#[async_trait]
impl ProviderFactory for DefaultProviderFactory {
    async fn create_providers(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<String, ProviderHandle>> {
        let registry = ProviderRegistry::new();
        let mut providers = HashMap::new();
        for (key, entry) in &loaded.global.providers.entries {
            let provider = registry
                .create_provider(entry)
                .wrap_err_with(|| format!("failed to initialize provider '{key}'"))?;
            let metadata = registry
                .metadata_for_kind(entry.kind())
                .wrap_err_with(|| format!("failed to read metadata for provider '{key}'"))?;
            providers.insert(key.clone(), ProviderHandle { provider, metadata });
        }
        Ok(providers)
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
    pub runtimes: HashMap<String, AgentRuntime>,
    pub safe_error_reply: String,
}

pub(crate) async fn assemble_runtime_state(
    cli: &Cli,
    loaded: &LoadedConfig,
    app_paths: &AppPaths,
    deps: &RuntimeDependencies,
) -> color_eyre::Result<RuntimeState> {
    let providers_by_key = deps.provider_factory.create_providers(loaded).await?;

    let mut memory_by_agent: HashMap<String, MemoryStore> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = deps.memory_factory.create_memory(agent, loaded).await?;
        memory_by_agent.insert(agent.id.clone(), memory);
    }

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::channel::<InboundMessage>(1_024);

    let summon_agents: HashMap<String, PathBuf> = loaded
        .global
        .agents
        .list
        .iter()
        .map(|agent| (agent.id.clone(), agent.workspace.clone()))
        .collect();
    let mut runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = memory_by_agent.get(&agent.id).cloned().ok_or_else(|| {
            color_eyre::eyre::eyre!("missing memory store for configured agent '{}'", agent.id)
        })?;
        let agent_config = load_agent_config_for_workspace(&agent.workspace)
            .wrap_err_with(|| format!("failed to load agent.yaml for agent '{}'", agent.id))?;
        let provider_key = agent_config
            .provider
            .as_deref()
            .unwrap_or(loaded.global.providers.default.as_str());
        if provider_key.trim().is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "agent '{}' has an empty provider key in agent.yaml",
                agent.id
            ));
        }
        let provider_handle = providers_by_key.get(provider_key).ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "agent '{}' references unknown provider key '{}'",
                agent.id,
                provider_key
            )
        })?;
        let system_prompt = load_system_prompt_for_workspace(&agent.workspace).wrap_err_with(|| {
            format!(
                "failed to assemble layered system prompt for agent '{}'",
                agent.id
            )
        })?;
        let tooling = build_tool_registry_for_agent(
            &agent.id,
            &agent_config,
            &agent.workspace,
            &app_paths.base_dir,
        )
        .wrap_err_with(|| format!("failed to load skill tools for agent '{}'", agent.id))?;
        info!(
            agent_id = %agent.id,
            status = "loaded",
            "agent skill tools loaded"
        );
        runtimes.insert(
            agent.id.clone(),
            AgentRuntime::new(AgentRuntimeConfig {
                agent_id: agent.id.clone(),
                runtime_config: loaded.global.runtime.clone(),
                agent_config: agent_config.clone(),
                provider: Arc::clone(&provider_handle.provider),
                provider_supports_native_tools: provider_handle.metadata.supports_native_tools,
                memory,
                summon_agents: summon_agents.clone(),
                summon_memories: memory_by_agent.clone(),
                workspace_root: agent.workspace.clone(),
                app_base_dir: app_paths.base_dir.clone(),
                system_prompt,
                tool_registry: tooling.tool_registry,
                skill_tool_names: tooling.skill_tool_names,
                max_steps: cli.max_steps,
                completion_tx: Some(gateway_tx.clone()),
            }),
        );
    }

    let channels = deps.channel_factory.create_channels(loaded).await?;
    let gateway = Gateway::new(
        channels,
        loaded.global.inbound.clone(),
        gateway_tx,
        gateway_rx,
    );

    Ok(RuntimeState {
        gateway,
        runtimes,
        safe_error_reply: loaded.global.runtime.safe_error_reply.clone(),
    })
}

pub(crate) fn agent_workspace_memory_paths(workspace: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let memory_dir = workspace.join(".simpleclaw").join("memory");
    let short_term_path = memory_dir.join("lraf.db");
    let long_term_path = memory_dir.join("lraf_long_term.db");
    (memory_dir, short_term_path, long_term_path)
}
