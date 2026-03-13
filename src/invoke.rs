use std::sync::{Arc, Weak};

use async_trait::async_trait;

use crate::agent::AgentDirectory;
use crate::error::FrameworkError;
use crate::providers::{Message, Role};
use crate::react::{ReactLoop, RunParams};
use crate::tools::{
    AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, WorkerInvokeRequest,
};

/// Implements agent-to-agent invocation by looking up configs in the
/// [`AgentDirectory`] and recursing through the [`ReactLoop`].
///
/// Constructed once at composition time and held by [`ReactLoop`].
pub(crate) struct DirectAgentInvoker {
    react_loop: Weak<ReactLoop>,
    agents: Arc<AgentDirectory>,
    async_tool_runs: Arc<AsyncToolRunManager>,
}

impl DirectAgentInvoker {
    pub fn new(
        react_loop: Weak<ReactLoop>,
        agents: Arc<AgentDirectory>,
        async_tool_runs: Arc<AsyncToolRunManager>,
    ) -> Self {
        Self {
            react_loop,
            agents,
            async_tool_runs,
        }
    }
}

#[async_trait]
impl AgentInvoker for DirectAgentInvoker {
    async fn invoke_agent(
        &self,
        request: AgentInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError> {
        let target_config = self
            .agents
            .config(&request.target_agent_id)
            .ok_or_else(|| {
                FrameworkError::Tool(format!("unknown agent: {}", request.target_agent_id))
            })?;
        let memory = self
            .agents
            .memory(&request.target_agent_id)
            .ok_or_else(|| {
                FrameworkError::Tool(format!("no memory for agent: {}", request.target_agent_id))
            })?;
        let effective_max_steps = target_config.effective_execution.max_steps;
        let delegated_exec_enabled = target_config
            .agent_config
            .tools
            .exec
            .clone()
            .unwrap_or_default()
            .enabled;
        let delegated_tool_registry = target_config
            .tool_registry
            .without_names(&["summon", "task", "background"])
            .with_async_disabled_if(delegated_exec_enabled);
        let execution_env = target_config.effective_execution.resolved_env()?;
        let params = RunParams {
            provider_key: &target_config.provider_key,
            system_prompt: &target_config.system_prompt,
            agent_id: &request.target_agent_id,
            agent_name: &target_config.agent_name,
            session_id: &request.session_id,
            max_steps: effective_max_steps,
            history_messages: target_config.effective_execution.history_messages as usize,
            execution_env: &execution_env,
            memory: memory.as_ref(),
            persona_root: &target_config.persona_root,
            workspace_root: &target_config.workspace_root,
            user_id: &request.user_id,
            owner_ids: &target_config.owner_ids,
            async_tool_runs: &self.async_tool_runs,
            approval_requester: request.approval_requester,
            tool_registry: &delegated_tool_registry,
            gateway: None,
            completion_tx: None,
            completion_route: None,
            source_message_id: None,
            on_text_delta: None,
            allow_async_tools: false,
        };
        self.react_loop
            .upgrade()
            .ok_or_else(|| FrameworkError::Tool("react loop unavailable".to_owned()))?
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
            .map(|outcome| InvokeOutcome {
                reply: outcome.reply,
                tool_calls: outcome.tool_calls,
            })
    }

    async fn invoke_worker(
        &self,
        request: WorkerInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError> {
        let current_config = self
            .agents
            .config(&request.current_agent_id)
            .ok_or_else(|| FrameworkError::Tool("current agent config unavailable".to_owned()))?;
        let memory = self
            .agents
            .memory(&request.current_agent_id)
            .ok_or_else(|| FrameworkError::Tool("current agent memory unavailable".to_owned()))?;
        let worker_tool_registry = current_config
            .tool_registry
            .without_names(&["summon", "task", "background", "memorize", "forget"])
            .with_async_disabled_if(
                current_config
                    .agent_config
                    .tools
                    .exec
                    .clone()
                    .unwrap_or_default()
                    .enabled,
            );
        let execution_env = current_config.effective_execution.resolved_env()?;
        let params = RunParams {
            provider_key: &current_config.provider_key,
            system_prompt: "You are a task worker. Complete the assigned task and return a concise result.",
            agent_id: "task-worker",
            agent_name: "Task Worker",
            session_id: &request.session_id,
            max_steps: request
                .max_steps_override
                .unwrap_or(current_config.effective_execution.max_steps),
            history_messages: current_config.effective_execution.history_messages as usize,
            execution_env: &execution_env,
            memory: memory.as_ref(),
            persona_root: &current_config.persona_root,
            workspace_root: &current_config.workspace_root,
            user_id: &request.user_id,
            owner_ids: &current_config.owner_ids,
            async_tool_runs: &self.async_tool_runs,
            approval_requester: request.approval_requester,
            tool_registry: &worker_tool_registry,
            gateway: None,
            completion_tx: None,
            completion_route: None,
            source_message_id: None,
            on_text_delta: None,
            allow_async_tools: false,
        };
        self.react_loop
            .upgrade()
            .ok_or_else(|| FrameworkError::Tool("react loop unavailable".to_owned()))?
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
            .map(|outcome| InvokeOutcome {
                reply: outcome.reply,
                tool_calls: outcome.tool_calls,
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::DirectAgentInvoker;
    use crate::agent::{AgentDirectory, AgentRuntimeConfig};
    use crate::approval::UnavailableApprovalRequester;
    use crate::config::{AgentInnerConfig, ExecutionDefaultsConfig, MemoryRecallConfig};
    use crate::error::FrameworkError;
    use crate::memory::{
        DynMemory, LongTermFactSummary, LongTermForgetResult, MemorizeResult, Memory,
        MemoryRecallHit, MemoryStoreScope, StoredMessage, StoredRole,
    };
    use crate::providers::{
        Message, Provider, ProviderFactory, ProviderResponse, Role, ToolDefinition,
    };
    use crate::react::ReactLoop;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, WorkerInvokeRequest,
        default_factory,
    };

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

    struct RecordingProvider {
        response: ProviderResponse,
        calls: AtomicUsize,
        tools_seen: Mutex<Vec<Vec<String>>>,
        histories_seen: Mutex<Vec<Vec<Message>>>,
    }

    impl RecordingProvider {
        fn final_text(reply: &str) -> Self {
            Self {
                response: ProviderResponse {
                    output_text: Some(reply.to_owned()),
                    tool_calls: Vec::new(),
                },
                calls: AtomicUsize::new(0),
                tools_seen: Mutex::new(Vec::new()),
                histories_seen: Mutex::new(Vec::new()),
            }
        }

        fn empty() -> Self {
            Self {
                response: ProviderResponse {
                    output_text: None,
                    tool_calls: Vec::new(),
                },
                calls: AtomicUsize::new(0),
                tools_seen: Mutex::new(Vec::new()),
                histories_seen: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        async fn tools_seen(&self) -> Vec<Vec<String>> {
            self.tools_seen.lock().await.clone()
        }

        async fn histories_seen(&self) -> Vec<Vec<Message>> {
            self.histories_seen.lock().await.clone()
        }
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        async fn generate(
            &self,
            _system_prompt: &str,
            history: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.tools_seen.lock().await.push(
                tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>(),
            );
            self.histories_seen.lock().await.push(history.to_vec());
            Ok(self.response.clone())
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

    struct NoopInvoker;

    #[async_trait]
    impl AgentInvoker for NoopInvoker {
        async fn invoke_agent(
            &self,
            _request: AgentInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool("unexpected invoke_agent".to_owned()))
        }

        async fn invoke_worker(
            &self,
            _request: WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool("unexpected invoke_worker".to_owned()))
        }
    }

    fn test_agent_config() -> AgentRuntimeConfig {
        let mut agent_config = AgentInnerConfig::default();
        agent_config.tools = agent_config.tools.with_disabled(&["cron"]);
        let tool_registry = default_factory()
            .build_registry(&agent_config.tools, &[])
            .expect("tool registry should build");
        AgentRuntimeConfig {
            agent_id: "default".to_owned(),
            agent_name: "Default".to_owned(),
            provider_key: "default".to_owned(),
            effective_execution: ExecutionDefaultsConfig {
                max_steps: 4,
                history_messages: 6,
                ..ExecutionDefaultsConfig::default()
            },
            owner_ids: vec!["owner-1".to_owned()],
            agent_config,
            tool_registry,
            persona_root: PathBuf::from("/tmp/simpleclaw-invoke-persona"),
            workspace_root: PathBuf::from("/tmp/simpleclaw-invoke-workspace"),
            app_base_dir: PathBuf::from("/tmp/simpleclaw-invoke-app"),
            system_prompt: "system prompt".to_owned(),
        }
    }

    fn test_react_loop(provider: Arc<dyn Provider>) -> Arc<ReactLoop> {
        Arc::new(ReactLoop::new(
            ProviderFactory::from_parts(HashMap::from([(
                "default".to_owned(),
                (
                    Box::new(ForwardProvider { inner: provider }) as Box<dyn Provider>,
                    true,
                ),
            )])),
            Arc::new(NoopInvoker),
        ))
    }

    fn test_directory(with_memory: bool) -> Arc<AgentDirectory> {
        let config = test_agent_config();
        let memories = if with_memory {
            HashMap::from([("default".to_owned(), Arc::new(NoopMemory) as DynMemory)])
        } else {
            HashMap::new()
        };
        Arc::new(AgentDirectory::new(
            HashMap::from([("default".to_owned(), config)]),
            memories,
        ))
    }

    #[tokio::test]
    async fn invoke_agent_returns_unknown_agent_error() {
        let provider: Arc<dyn Provider> = Arc::new(RecordingProvider::final_text("unused"));
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&test_react_loop(provider)),
            Arc::new(AgentDirectory::new(HashMap::new(), HashMap::new())),
            Arc::new(AsyncToolRunManager::new()),
        );

        let err = invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: "missing".to_owned(),
                session_id: "sess-1".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "hello".to_owned(),
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .err()
            .expect("missing agent should fail");

        assert!(err.to_string().contains("unknown agent: missing"));
    }

    #[tokio::test]
    async fn invoke_agent_returns_missing_memory_error() {
        let provider: Arc<dyn Provider> = Arc::new(RecordingProvider::final_text("unused"));
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&test_react_loop(provider)),
            test_directory(false),
            Arc::new(AsyncToolRunManager::new()),
        );

        let err = invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: "default".to_owned(),
                session_id: "sess-1".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "hello".to_owned(),
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .err()
            .expect("missing memory should fail");

        assert!(err.to_string().contains("no memory for agent: default"));
    }

    #[tokio::test]
    async fn invoke_agent_runs_with_target_agent_prompt_and_history() {
        let provider_impl = Arc::new(RecordingProvider::final_text("agent reply"));
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let react_loop = test_react_loop(provider);
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&react_loop),
            test_directory(true),
            Arc::new(AsyncToolRunManager::new()),
        );

        let outcome = invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: "default".to_owned(),
                session_id: "sess-1".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "delegate this".to_owned(),
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .expect("invoke_agent should succeed");

        assert_eq!(outcome.reply, "agent reply");
        let histories = provider_impl.histories_seen().await;
        assert_eq!(histories.len(), 1);
        assert_eq!(histories[0].len(), 1);
        assert_eq!(histories[0][0].role, Role::User);
        assert_eq!(histories[0][0].content, "delegate this");
    }

    #[tokio::test]
    async fn invoke_agent_disables_recursive_and_background_tools_in_tool_specs() {
        let provider_impl = Arc::new(RecordingProvider::final_text("agent reply"));
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let react_loop = test_react_loop(provider);
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&react_loop),
            test_directory(true),
            Arc::new(AsyncToolRunManager::new()),
        );

        invoker
            .invoke_agent(AgentInvokeRequest {
                target_agent_id: "default".to_owned(),
                session_id: "sess-1".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "delegate this".to_owned(),
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .expect("invoke_agent should succeed");

        let tool_sets = provider_impl.tools_seen().await;
        assert_eq!(tool_sets.len(), 1);
        let tools = &tool_sets[0];
        assert!(!tools.iter().any(|name| name == "summon"));
        assert!(!tools.iter().any(|name| name == "task"));
        assert!(!tools.iter().any(|name| name == "background"));
    }

    #[tokio::test]
    async fn invoke_worker_disables_recursive_tools_in_tool_specs() {
        let provider_impl = Arc::new(RecordingProvider::final_text("worker reply"));
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let react_loop = test_react_loop(provider);
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&react_loop),
            test_directory(true),
            Arc::new(AsyncToolRunManager::new()),
        );

        let outcome = invoker
            .invoke_worker(WorkerInvokeRequest {
                current_agent_id: "default".to_owned(),
                session_id: "sess-2".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "do work".to_owned(),
                max_steps_override: None,
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .expect("invoke_worker should succeed");

        assert_eq!(outcome.reply, "worker reply");
        let tool_sets = provider_impl.tools_seen().await;
        assert_eq!(tool_sets.len(), 1);
        let tools = &tool_sets[0];
        assert!(!tools.iter().any(|name| name == "summon"));
        assert!(!tools.iter().any(|name| name == "task"));
        assert!(!tools.iter().any(|name| name == "background"));
        assert!(!tools.iter().any(|name| name == "memorize"));
        assert!(!tools.iter().any(|name| name == "forget"));
        assert!(tools.iter().any(|name| name == "clock"));
    }

    #[tokio::test]
    async fn invoke_worker_uses_max_steps_override() {
        let provider_impl = Arc::new(RecordingProvider::empty());
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let react_loop = test_react_loop(provider);
        let invoker = DirectAgentInvoker::new(
            Arc::downgrade(&react_loop),
            test_directory(true),
            Arc::new(AsyncToolRunManager::new()),
        );

        let outcome = invoker
            .invoke_worker(WorkerInvokeRequest {
                current_agent_id: "default".to_owned(),
                session_id: "sess-3".to_owned(),
                user_id: "user-1".to_owned(),
                prompt: "loop".to_owned(),
                max_steps_override: Some(2),
                approval_requester: Arc::new(UnavailableApprovalRequester),
            })
            .await
            .expect("invoke_worker should succeed");

        assert_eq!(outcome.reply, "max_steps reached without final response");
        assert_eq!(provider_impl.call_count(), 2);
    }
}
