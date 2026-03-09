use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

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
use crate::tools::builtin::cron::{CronStore, CronTool};
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

pub(crate) struct RuntimeDependencies {
    pub provider_factory_builder: Arc<dyn ProviderFactoryBuilder>,
    pub memory_factory: Arc<dyn MemoryFactory>,
    pub channel_factory: Arc<dyn ChannelFactory>,
    pub tool_factory_builder: Arc<dyn ToolFactoryBuilder>,
    pub skill_factory_builder: Arc<dyn SkillFactoryBuilder>,
    pub process_manager_factory: Arc<dyn ProcessManagerFactory>,
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

pub(crate) struct RuntimeState {
    pub gateway: Arc<Gateway>,
    pub runtimes: HashMap<String, AgentRuntime>,
    pub context: Arc<RuntimeContext>,
}

pub(crate) struct RuntimeServices {
    _gateway_listeners: crate::gateway::GatewayListeners,
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

    let cron_store = Arc::new(std::sync::Mutex::new(CronStore::open(
        &app_paths.cron_db_path,
    )?));

    let mut tool_factory = deps.tool_factory_builder.create_tool_factory();
    tool_factory.register_builtin(Box::new(CronTool::new(Arc::clone(&cron_store))));
    let directory = Arc::new(AgentDirectory::new(agent_configs_map, memories_map));
    let mut runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    for (agent_id, config) in directory.iter_configs() {
        runtimes.insert(agent_id.clone(), AgentRuntime::new(config.clone()));
    }

    let process_manager = deps.process_manager_factory.create_process_manager();
    let react_loop = Arc::new_cyclic(|react_loop: &Weak<ReactLoop>| {
        let invoker: Arc<dyn crate::tools::AgentInvoker> = Arc::new(DirectAgentInvoker::new(
            react_loop.clone(),
            Arc::clone(&directory),
            Arc::clone(&process_manager),
        ));
        ReactLoop::new(provider_factory, tool_factory, skill_factory, invoker)
    });

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::channel::<InboundMessage>(1_024);

    let channels = deps.channel_factory.create_channels(loaded).await?;
    let gateway = Arc::new(Gateway::new(channels, loaded.global.gateway.routing.clone()));

    let context = Arc::new(RuntimeContext {
        react_loop,
        gateway: Arc::clone(&gateway),
        agents: directory,
        process_manager,
        cron_store,
        completion_tx: gateway_tx,
        safe_error_reply: loaded.global.execution.defaults.safe_error_reply.clone(),
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

pub(crate) fn start_runtime_services(state: &RuntimeState) -> RuntimeServices {
    RuntimeServices {
        _gateway_listeners: state.gateway.start(state.context.completion_tx.clone()),
    }
}

pub(crate) fn agent_workspace_memory_paths(workspace: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let memory_dir = workspace.join(".simpleclaw").join("memory");
    let short_term_path = memory_dir.join("short_term_memory.db");
    let long_term_path = memory_dir.join("long_term_memory.db");
    (memory_dir, short_term_path, long_term_path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;

    use super::{
        ChannelFactory, MemoryFactory, ProviderFactoryBuilder, RuntimeDependencies,
        agent_workspace_memory_paths, assemble_runtime_state,
    };
    use crate::config::{
        AgentEntryConfig, GatewayChannelKind, GlobalConfig, LoadedConfig, MemoryRecallConfig,
    };
    use crate::memory::{
        DynMemory, LongTermFactSummary, LongTermForgetResult, MemorizeResult, Memory, MemoryRecallHit,
        MemoryStoreScope, StoredMessage, StoredRole,
    };
    use crate::paths::AppPaths;
    use crate::providers::{Provider, ProviderFactory, ProviderResponse, ToolDefinition};
    use crate::error::FrameworkError;

    #[derive(Default)]
    struct NoopMemory;

    #[async_trait]
    impl Memory for NoopMemory {
        async fn append_message(
            &self,
            _session_id: &str,
            _role: StoredRole,
            _content: &str,
            _username: Option<&str>,
        ) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn semantic_query_combined(
            &self,
            _session_id: &str,
            _query: &str,
            _top_k: usize,
            _history_window: usize,
            _scope: MemoryStoreScope,
        ) -> Result<Vec<String>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn query_recall_hits(
            &self,
            _session_id: &str,
            _query: &str,
            _config: &MemoryRecallConfig,
            _history_window: usize,
            _scope: MemoryStoreScope,
            _prefer_long_term: bool,
        ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn semantic_forget_long_term(
            &self,
            _query: &str,
            _similarity_threshold: f32,
            _max_matches: usize,
            _kind_filter: Option<&str>,
            _commit: bool,
        ) -> Result<LongTermForgetResult, FrameworkError> {
            Ok(LongTermForgetResult {
                matches: Vec::new(),
                deleted_count: 0,
                similarity_threshold: 0.0,
                max_matches: 0,
                kind_filter: None,
            })
        }

        async fn recent_messages(
            &self,
            _session_id: &str,
            _limit: usize,
        ) -> Result<Vec<StoredMessage>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn memorize(
            &self,
            _session_id: &str,
            _content: &str,
            _kind: &str,
            _importance: u8,
        ) -> Result<MemorizeResult, FrameworkError> {
            Ok(MemorizeResult::Inserted)
        }

        async fn list_long_term_facts(
            &self,
            _kind_filter: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<LongTermFactSummary>, FrameworkError> {
            Ok(Vec::new())
        }
    }

    struct StubProvider;

    #[async_trait]
    impl Provider for StubProvider {
        async fn generate(
            &self,
            _system_prompt: &str,
            _history: &[crate::providers::Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            Ok(ProviderResponse {
                output_text: Some("ok".to_owned()),
                tool_calls: Vec::new(),
            })
        }
    }

    struct StaticProviderBuilder {
        include_default: bool,
    }

    #[async_trait]
    impl ProviderFactoryBuilder for StaticProviderBuilder {
        async fn create_provider_factory(
            &self,
            _loaded: &LoadedConfig,
        ) -> color_eyre::Result<ProviderFactory> {
            let entries = if self.include_default {
                HashMap::from([(
                    "default".to_owned(),
                    (Box::new(StubProvider) as Box<dyn Provider>, true),
                )])
            } else {
                HashMap::new()
            };
            Ok(ProviderFactory::from_parts(entries))
        }
    }

    struct StaticMemoryFactory;

    #[async_trait]
    impl MemoryFactory for StaticMemoryFactory {
        async fn create_memory(
            &self,
            _agent: &AgentEntryConfig,
            _loaded: &LoadedConfig,
        ) -> color_eyre::Result<DynMemory> {
            Ok(Arc::new(NoopMemory))
        }
    }

    struct EmptyChannelFactory;

    #[async_trait]
    impl ChannelFactory for EmptyChannelFactory {
        async fn create_channels(
            &self,
            _loaded: &LoadedConfig,
        ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn crate::channels::Channel>>> {
            Ok(HashMap::new())
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic enough for tests")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn test_loaded_config() -> LoadedConfig {
        let workspace = temp_dir("composition_workspace");
        let mut global = GlobalConfig::default();
        global.agents.list = vec![AgentEntryConfig {
            id: "default".to_owned(),
            name: "Default".to_owned(),
            workspace,
            config: crate::config::AgentInnerConfig::default(),
        }];
        LoadedConfig { global }
    }

    fn test_app_paths() -> AppPaths {
        let base_dir = temp_dir("composition_app");
        AppPaths {
            base_dir: base_dir.clone(),
            config_path: base_dir.join("config.yaml"),
            secrets_path: base_dir.join("secrets.yaml"),
            db_path: base_dir.join("db/short.db"),
            long_term_db_path: base_dir.join("db/long.db"),
            cron_db_path: base_dir.join("db/cron.db"),
            fastembed_cache_dir: base_dir.join(".fastembed"),
            logs_dir: base_dir.join("logs"),
            log_path: base_dir.join("logs/service.log"),
            run_dir: base_dir.join("run"),
            pid_path: base_dir.join("run/service.pid"),
        }
    }

    #[test]
    fn agent_workspace_memory_paths_uses_simpleclaw_memory_layout() {
        let workspace = PathBuf::from("/tmp/workspace");
        let (memory_dir, short_term_path, long_term_path) = agent_workspace_memory_paths(&workspace);

        assert_eq!(memory_dir, workspace.join(".simpleclaw").join("memory"));
        assert_eq!(short_term_path, memory_dir.join("short_term_memory.db"));
        assert_eq!(long_term_path, memory_dir.join("long_term_memory.db"));
    }

    #[tokio::test]
    async fn assemble_runtime_state_builds_runtime_directory_and_safe_reply() {
        let loaded = test_loaded_config();
        let app_paths = test_app_paths();
        let deps = RuntimeDependencies {
            provider_factory_builder: Arc::new(StaticProviderBuilder {
                include_default: true,
            }),
            memory_factory: Arc::new(StaticMemoryFactory),
            channel_factory: Arc::new(EmptyChannelFactory),
            ..RuntimeDependencies::default()
        };

        let (state, _rx) = assemble_runtime_state(&loaded, &app_paths, &deps)
            .await
            .expect("runtime state should assemble");

        assert_eq!(state.runtimes.len(), 1);
        assert!(state.runtimes.contains_key("default"));
        assert_eq!(state.context.safe_error_reply, loaded.global.execution.defaults.safe_error_reply);
        assert!(state.context.agents.config("default").is_some());
        assert!(state.context.agents.memory("default").is_some());
    }

    #[tokio::test]
    async fn assemble_runtime_state_rejects_empty_provider_key() {
        let mut loaded = test_loaded_config();
        loaded.global.agents.list[0].config.provider = Some("   ".to_owned());
        let app_paths = test_app_paths();
        let deps = RuntimeDependencies {
            provider_factory_builder: Arc::new(StaticProviderBuilder {
                include_default: true,
            }),
            memory_factory: Arc::new(StaticMemoryFactory),
            channel_factory: Arc::new(EmptyChannelFactory),
            ..RuntimeDependencies::default()
        };

        let err = assemble_runtime_state(&loaded, &app_paths, &deps)
            .await
            .err()
            .expect("empty provider key should fail");

        assert!(err.to_string().contains("empty provider key"));
    }

    #[tokio::test]
    async fn assemble_runtime_state_rejects_missing_provider_registration() {
        let loaded = test_loaded_config();
        let app_paths = test_app_paths();
        let deps = RuntimeDependencies {
            provider_factory_builder: Arc::new(StaticProviderBuilder {
                include_default: false,
            }),
            memory_factory: Arc::new(StaticMemoryFactory),
            channel_factory: Arc::new(EmptyChannelFactory),
            ..RuntimeDependencies::default()
        };

        let err = assemble_runtime_state(&loaded, &app_paths, &deps)
            .await
            .err()
            .expect("missing provider should fail");

        assert!(err.to_string().contains("unknown provider key 'default'"));
    }
}
