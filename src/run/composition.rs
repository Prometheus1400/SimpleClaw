use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use color_eyre::eyre::WrapErr;
use tracing::info;

use crate::agent::{
    AgentDirectory, AgentRuntime, AgentRuntimeConfig, RuntimeContext,
    load_system_prompt_for_workspace,
};
use crate::channels::{Channel, DiscordChannel, InboundMessage};
use crate::config::{AgentEntryConfig, GatewayChannelKind, LoadedConfig};
use crate::gateway::Gateway;
use crate::invoke::DirectAgentInvoker;
use crate::memory::{DynMemory, MemoryStore};
use crate::paths::AppPaths;
use crate::providers::ProviderFactory;
use crate::react::ReactLoop;
use crate::tools::skill::SkillFactory;
use crate::tools::{ProcessManager, ToolFactory, default_factory};

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
    ) -> color_eyre::Result<DynMemory>;
}

#[async_trait]
pub(crate) trait ChannelFactory: Send + Sync {
    async fn create_channels(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>>;
}

pub(crate) trait ToolFactoryBuilder: Send + Sync {
    fn create_tool_factory(&self) -> ToolFactory;
}

pub(crate) trait SkillFactoryBuilder: Send + Sync {
    fn create_skill_factory(&self, app_paths: &AppPaths) -> SkillFactory;
}

pub(crate) trait ProcessManagerFactory: Send + Sync {
    fn create_process_manager(&self) -> Arc<ProcessManager>;
}

pub(crate) trait ReactLoopFactory: Send + Sync {
    fn create_react_loop(
        &self,
        provider_factory: ProviderFactory,
        tool_factory: ToolFactory,
        skill_factory: SkillFactory,
    ) -> Arc<ReactLoop>;
}

pub(crate) struct RuntimeDependencies {
    pub provider_factory_builder: Arc<dyn ProviderFactoryBuilder>,
    pub memory_factory: Arc<dyn MemoryFactory>,
    pub channel_factory: Arc<dyn ChannelFactory>,
    pub tool_factory_builder: Arc<dyn ToolFactoryBuilder>,
    pub skill_factory_builder: Arc<dyn SkillFactoryBuilder>,
    pub process_manager_factory: Arc<dyn ProcessManagerFactory>,
    pub react_loop_factory: Arc<dyn ReactLoopFactory>,
}

impl Default for RuntimeDependencies {
    fn default() -> Self {
        Self {
            provider_factory_builder: Arc::new(DefaultProviderFactoryBuilder),
            memory_factory: Arc::new(DefaultMemoryFactory),
            channel_factory: Arc::new(DefaultChannelFactory),
            tool_factory_builder: Arc::new(DefaultToolFactoryBuilder),
            skill_factory_builder: Arc::new(DefaultSkillFactoryBuilder),
            process_manager_factory: Arc::new(DefaultProcessManagerFactory),
            react_loop_factory: Arc::new(DefaultReactLoopFactory),
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
    ) -> color_eyre::Result<DynMemory> {
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
        .map(|memory| Arc::new(memory) as DynMemory)
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
        for (kind, config) in &loaded.global.gateway.channels {
            if !config.enabled {
                continue;
            }
            let channel: Arc<dyn Channel> = match kind {
                GatewayChannelKind::Discord => Arc::new(
                    DiscordChannel::from_config(config)
                        .await
                        .wrap_err("failed to initialize discord channel")?,
                ),
            };
            channels.insert(*kind, channel);
        }
        Ok(channels)
    }
}

struct DefaultToolFactoryBuilder;

impl ToolFactoryBuilder for DefaultToolFactoryBuilder {
    fn create_tool_factory(&self) -> ToolFactory {
        default_factory()
    }
}

struct DefaultSkillFactoryBuilder;

impl SkillFactoryBuilder for DefaultSkillFactoryBuilder {
    fn create_skill_factory(&self, app_paths: &AppPaths) -> SkillFactory {
        SkillFactory::new(app_paths.base_dir.clone())
    }
}

struct DefaultProcessManagerFactory;

impl ProcessManagerFactory for DefaultProcessManagerFactory {
    fn create_process_manager(&self) -> Arc<ProcessManager> {
        Arc::new(ProcessManager::new())
    }
}

struct DefaultReactLoopFactory;

impl ReactLoopFactory for DefaultReactLoopFactory {
    fn create_react_loop(
        &self,
        provider_factory: ProviderFactory,
        tool_factory: ToolFactory,
        skill_factory: SkillFactory,
    ) -> Arc<ReactLoop> {
        Arc::new(ReactLoop::new(
            provider_factory,
            tool_factory,
            skill_factory,
        ))
    }
}

pub(crate) struct RuntimeState {
    pub gateway: Arc<Gateway>,
    pub runtimes: HashMap<String, AgentRuntime>,
    pub context: Arc<RuntimeContext>,
}

pub(crate) async fn assemble_runtime_state(
    loaded: &LoadedConfig,
    app_paths: &AppPaths,
    deps: &RuntimeDependencies,
) -> color_eyre::Result<(RuntimeState, tokio::sync::mpsc::Receiver<InboundMessage>)> {
    let provider_factory = deps
        .provider_factory_builder
        .create_provider_factory(loaded)
        .await?;

    let mut memories_map: HashMap<String, DynMemory> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = deps.memory_factory.create_memory(agent, loaded).await?;
        memories_map.insert(agent.id.clone(), memory);
    }

    let mut agent_configs_map: HashMap<String, AgentRuntimeConfig> = HashMap::new();
    let mut skill_factory = deps.skill_factory_builder.create_skill_factory(app_paths);

    for agent in &loaded.global.agents.list {
        let agent_config = agent.config.clone();
        let provider_key = agent_config
            .provider
            .as_deref()
            .unwrap_or(loaded.global.providers.default.as_str())
            .trim()
            .to_owned();
        let effective_execution = loaded
            .global
            .execution
            .defaults
            .merge_with_overrides(&agent_config.execution);
        if provider_key.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "agent '{}' has an empty provider key in config.yaml",
                agent.id
            ));
        }
        provider_factory
            .get(&provider_key)
            .map_err(color_eyre::Report::from)?;

        let system_prompt =
            load_system_prompt_for_workspace(&agent.workspace).wrap_err_with(|| {
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
                effective_execution,
                owner_ids: loaded.global.execution.owner_ids.clone(),
                agent_config,
                workspace_root: agent.workspace.clone(),
                app_base_dir: app_paths.base_dir.clone(),
                system_prompt,
            },
        );
    }

    let tool_factory = deps.tool_factory_builder.create_tool_factory();
    let react_loop =
        deps.react_loop_factory
            .create_react_loop(provider_factory, tool_factory, skill_factory);

    let directory = Arc::new(AgentDirectory::new(agent_configs_map, memories_map));
    let mut runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    for (agent_id, config) in directory.iter_configs() {
        runtimes.insert(agent_id.clone(), AgentRuntime::new(config.clone()));
    }

    let process_manager = deps.process_manager_factory.create_process_manager();

    let invoker: Arc<dyn crate::tools::AgentInvoker> = Arc::new(DirectAgentInvoker::new(
        Arc::clone(&react_loop),
        Arc::clone(&directory),
        Arc::clone(&process_manager),
    ));
    react_loop.set_invoker(invoker);

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::channel::<InboundMessage>(1_024);

    let channels = deps.channel_factory.create_channels(loaded).await?;
    let gateway = Arc::new(Gateway::new(
        channels,
        loaded.global.gateway.routing.clone(),
        gateway_tx.clone(),
    ));

    let context = Arc::new(RuntimeContext {
        react_loop,
        gateway: Arc::clone(&gateway),
        agents: directory,
        process_manager,
        completion_tx: gateway_tx,
        safe_error_reply: loaded.global.execution.defaults.safe_error_reply.clone(),
        tool_call_transparency: loaded.global.execution.defaults.transparency.tool_calls,
    });

    Ok((
        RuntimeState {
            gateway,
            runtimes,
            context,
        },
        gateway_rx,
    ))
}

pub(crate) fn agent_workspace_memory_paths(workspace: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let memory_dir = workspace.join(".simpleclaw").join("memory");
    let short_term_path = memory_dir.join("lraf.db");
    let long_term_path = memory_dir.join("lraf_long_term.db");
    (memory_dir, short_term_path, long_term_path)
}
