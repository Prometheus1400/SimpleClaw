use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channels::InboundMessage;
use crate::config::{AgentInnerConfig, ExecutionDefaultsConfig, TransparencyConfig};
use crate::error::FrameworkError;
use crate::memory::MemoryStoreScope;
use crate::memory::{DynMemory, MemoryHitStore, MemoryRecallHit, StoredRole};
use crate::prompt::PromptAssembler;
use crate::providers::{Message, Role};
use crate::react::{ReactLoop, RunOutcome, RunParams};
use crate::reply_policy::is_no_reply;
use crate::tools::{AgentToolRegistry, AsyncToolRunManager, CompletionRoute};

/// Groups declarative parameters needed for an `AgentRuntime`.
#[derive(Debug, Clone)]
pub struct AgentRuntimeConfig {
    pub agent_id: String,
    pub provider_key: String,
    pub effective_execution: ExecutionDefaultsConfig,
    pub owner_ids: Vec<String>,
    pub agent_config: AgentInnerConfig,
    pub tool_registry: AgentToolRegistry,
    pub persona_root: PathBuf,
    pub workspace_root: PathBuf,
    #[allow(dead_code)]
    pub app_base_dir: PathBuf,
    pub system_prompt: String,
}

#[derive(Clone)]
pub struct AgentDirectory {
    agent_configs: HashMap<String, AgentRuntimeConfig>,
    memories: HashMap<String, DynMemory>,
}

impl AgentDirectory {
    pub fn new(
        agent_configs: HashMap<String, AgentRuntimeConfig>,
        memories: HashMap<String, DynMemory>,
    ) -> Self {
        Self {
            agent_configs,
            memories,
        }
    }

    pub fn config(&self, agent_id: &str) -> Option<&AgentRuntimeConfig> {
        self.agent_configs.get(agent_id)
    }

    pub fn memory(&self, agent_id: &str) -> Option<&DynMemory> {
        self.memories.get(agent_id)
    }

    pub fn iter_configs(&self) -> impl Iterator<Item = (&String, &AgentRuntimeConfig)> {
        self.agent_configs.iter()
    }
}

#[derive(Clone)]
pub struct RuntimeContext {
    pub react_loop: Arc<ReactLoop>,
    pub gateway: Arc<crate::gateway::Gateway>,
    pub agents: Arc<AgentDirectory>,
    pub tool_runtime: Arc<ToolRuntime>,
}

#[derive(Clone)]
pub struct ToolRuntime {
    pub async_tool_runs: Arc<AsyncToolRunManager>,
    pub completion_tx: mpsc::Sender<InboundMessage>,
}

pub struct AgentRuntime {
    config: AgentRuntimeConfig,
}

impl AgentRuntime {
    pub fn new(config: AgentRuntimeConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &AgentRuntimeConfig {
        &self.config
    }

    pub fn transparency(&self) -> TransparencyConfig {
        self.config.effective_execution.transparency
    }

    #[tracing::instrument(
        name = "agent.run",
        skip(
            self,
            inbound,
            memory_session_id,
            context,
            on_text_delta
        ),
        fields(
            trace_id = %inbound.trace_id,
            session_id = %memory_session_id,
            agent_id = %self.config.agent_id,
            persona_root = %self.config.persona_root.display(),
            workspace_root = %self.config.workspace_root.display()
        )
    )]
    pub async fn run(
        &self,
        inbound: &InboundMessage,
        memory_session_id: &str,
        context: &RuntimeContext,
        on_text_delta: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    ) -> Result<RunOutcome, FrameworkError> {
        let execution_started = Instant::now();
        info!(status = "started", "agent execution");

        let memory = context
            .agents
            .memory(&self.config.agent_id)
            .cloned()
            .ok_or_else(|| {
                FrameworkError::Config(format!(
                    "missing memory store for agent '{}'",
                    self.config.agent_id
                ))
            })?;

        memory
            .append_message(
                memory_session_id,
                StoredRole::User,
                &inbound.content,
                Some(&inbound.username),
            )
            .await?;

        let history = self.seeded_history(&memory, memory_session_id).await?;
        let prompt_build = self
            .build_turn_system_prompt(&memory, memory_session_id, &inbound.content)
            .await;
        let system_prompt = inject_caller_context(&prompt_build.system_prompt, inbound);
        debug!(status = "history_loaded", "agent context");

        let effective_max_steps = self.config.effective_execution.max_steps;

        let params = RunParams {
            provider_key: &self.config.provider_key,
            system_prompt: &system_prompt,
            agent_id: &self.config.agent_id,
            session_id: memory_session_id,
            max_steps: effective_max_steps,
            history_messages: self.config.effective_execution.history_messages as usize,
            execution_env: self.config.effective_execution.resolved_env()?,
            memory: memory.clone(),
            persona_root: self.config.persona_root.clone(),
            workspace_root: self.config.workspace_root.clone(),
            user_id: inbound.user_id.clone(),
            owner_ids: self.config.owner_ids.clone(),
            async_tool_runs: Arc::clone(&context.tool_runtime.async_tool_runs),
            tool_registry: self.config.tool_registry.clone(),
            gateway: Some(Arc::clone(&context.gateway)),
            completion_tx: Some(context.tool_runtime.completion_tx.clone()),
            completion_route: Some(CompletionRoute {
                trace_id: inbound.trace_id.clone(),
                source_channel: inbound.source_channel,
                target_agent_id: self.config.agent_id.clone(),
                session_key: inbound.session_key.clone(),
                source_message_id: inbound.source_message_id.clone(),
                channel_id: inbound.channel_id.clone(),
                guild_id: inbound.guild_id.clone(),
                is_dm: inbound.is_dm,
            }),
            source_message_id: inbound.source_message_id.clone(),
            on_text_delta,
            allow_async_tools: true,
        };

        let mut outcome = match context.react_loop.run(params, history).await {
            Ok(outcome) => outcome,
            Err(err) => {
                error!(
                    status = "failed",
                    error_kind = "react_loop",
                    elapsed_ms = execution_started.elapsed().as_millis() as u64,
                    error = %err,
                    "agent execution"
                );
                return Err(err);
            }
        };
        outcome.memory_recall_used =
            prompt_build.memory_recall_short_hits + prompt_build.memory_recall_long_hits > 0;
        outcome.memory_recall_short_hits = prompt_build.memory_recall_short_hits;
        outcome.memory_recall_long_hits = prompt_build.memory_recall_long_hits;

        if !is_no_reply(&outcome.reply) {
            memory
                .append_message(
                    memory_session_id,
                    StoredRole::Assistant,
                    &outcome.reply,
                    None,
                )
                .await?;
        }
        info!(
            status = "completed",
            elapsed_ms = execution_started.elapsed().as_millis() as u64,
            "agent execution"
        );
        Ok(outcome)
    }

    pub async fn record_context(
        &self,
        inbound: &InboundMessage,
        memory_session_id: &str,
        directory: &AgentDirectory,
    ) -> Result<(), FrameworkError> {
        let memory = directory.memory(&self.config.agent_id).ok_or_else(|| {
            FrameworkError::Config(format!(
                "missing memory store for agent '{}'",
                self.config.agent_id
            ))
        })?;
        memory
            .append_message(
                memory_session_id,
                StoredRole::User,
                &inbound.content,
                Some(&inbound.username),
            )
            .await
    }

    async fn seeded_history(
        &self,
        memory: &DynMemory,
        session_id: &str,
    ) -> Result<Vec<Message>, FrameworkError> {
        let history_limit = self.config.effective_execution.history_messages as usize;
        let stored = memory.recent_messages(session_id, history_limit).await?;
        let mut history = Vec::with_capacity(stored.len());
        for item in stored {
            let role = match item.role {
                StoredRole::User => Role::User,
                StoredRole::Assistant => Role::Assistant,
                _ => continue,
            };
            let content = if matches!(role, Role::User) {
                if let Some(username) = item.username.as_deref().map(str::trim)
                    && !username.is_empty()
                {
                    format!("[{username}] {}", item.content)
                } else {
                    item.content
                }
            } else {
                item.content
            };
            history.push(Message::text(role, content));
        }
        Ok(history)
    }

    async fn build_turn_system_prompt(
        &self,
        memory: &DynMemory,
        session_id: &str,
        query: &str,
    ) -> PromptBuild {
        let config = self.config.effective_execution.memory_recall.normalized();
        if !config.enabled {
            return PromptBuild::without_recall(self.config.system_prompt.clone());
        }

        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return PromptBuild::without_recall(self.config.system_prompt.clone());
        }

        let hits = match memory
            .query_recall_hits(
                session_id,
                trimmed_query,
                &config,
                self.config.effective_execution.history_messages as usize,
                MemoryStoreScope::Combined,
                true,
            )
            .await
        {
            Ok(items) => items,
            Err(err) => {
                warn!(
                    status = "failed",
                    error_kind = "memory_recall_query",
                    error = %err,
                    "memory recall query"
                );
                return PromptBuild::without_recall(self.config.system_prompt.clone());
            }
        };

        if hits.is_empty() {
            debug!(status = "completed", "memory recall");
            return PromptBuild::without_recall(self.config.system_prompt.clone());
        }

        debug!(status = "completed", "memory recall");

        let recalled = format_recalled_memory(&hits, config.max_chars as usize);
        if recalled.section.is_empty() {
            return PromptBuild::without_recall(self.config.system_prompt.clone());
        }

        PromptBuild {
            system_prompt: format!("{}\n\n{}", self.config.system_prompt, recalled.section),
            memory_recall_short_hits: recalled.short_hits,
            memory_recall_long_hits: recalled.long_hits,
        }
    }
}

struct PromptBuild {
    system_prompt: String,
    memory_recall_short_hits: usize,
    memory_recall_long_hits: usize,
}

impl PromptBuild {
    fn without_recall(system_prompt: String) -> Self {
        Self {
            system_prompt,
            memory_recall_short_hits: 0,
            memory_recall_long_hits: 0,
        }
    }
}

fn format_recalled_memory(hits: &[MemoryRecallHit], max_chars: usize) -> PromptBuildMemorySection {
    if hits.is_empty() || max_chars == 0 {
        return PromptBuildMemorySection::default();
    }

    let base = "# POTENTIALLY RELEVANT MEMORY\nUse this as optional background context. It may be stale or incomplete; prioritize the current user message and conversation.";
    let mut section = base.to_owned();
    let mut short_hits = 0;
    let mut long_hits = 0;
    for (index, hit) in hits.iter().enumerate() {
        let line = format!(
            "\n{}. [{}] {}",
            index + 1,
            memory_hit_label(hit),
            hit.content.trim()
        );
        if section.len() + line.len() > max_chars {
            break;
        }
        section.push_str(&line);
        match hit.store {
            MemoryHitStore::LongTerm => long_hits += 1,
            MemoryHitStore::ShortTerm => short_hits += 1,
        }
    }

    if section == base {
        PromptBuildMemorySection::default()
    } else {
        PromptBuildMemorySection {
            section,
            short_hits,
            long_hits,
        }
    }
}

#[derive(Default)]
struct PromptBuildMemorySection {
    section: String,
    short_hits: usize,
    long_hits: usize,
}

fn memory_hit_label(hit: &MemoryRecallHit) -> String {
    match hit.store {
        MemoryHitStore::LongTerm => {
            format!("long-term/{}", hit.kind.as_deref().unwrap_or("general"))
        }
        MemoryHitStore::ShortTerm => {
            format!("short-term/{}", hit.kind.as_deref().unwrap_or("message"))
        }
    }
}

fn inject_caller_context(base: &str, inbound: &InboundMessage) -> String {
    let chat_type = if inbound.is_dm { "dm" } else { "group" };
    let platform = inbound.source_channel.as_str();
    let trigger_line = if inbound.user_id == "system" && inbound.username == "cron" {
        "\ntrigger: scheduled_cron"
    } else {
        ""
    };
    let guild_line = inbound
        .guild_id
        .as_ref()
        .map(|gid| format!("\nguild_id: {gid}"))
        .unwrap_or_default();
    let message_line = inbound
        .source_message_id
        .as_ref()
        .map(|message_id| format!("\nmessage_id: {message_id}"))
        .unwrap_or_default();
    format!(
        "{base}\n\n# CURRENT CONTEXT\nchat_type: {chat_type}\nplatform: {platform}\nchannel_id: {}{guild_line}{message_line}{trigger_line}\nSpeaker: **{}** (id: {})",
        inbound.channel_id, inbound.username, inbound.user_id
    )
}

pub(crate) fn load_system_prompt_for_persona(
    persona_root: &Path,
) -> Result<String, FrameworkError> {
    PromptAssembler::from_persona(persona_root)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::{Mutex, mpsc};

    use crate::channels::InboundMessage;
    use crate::channels::{Channel, ChannelInbound};
    use crate::config::{
        AgentInnerConfig, ChannelOutputMode, ExecutionDefaultsConfig, GatewayChannelKind,
        MemoryRecallConfig, RoutingConfig,
    };
    use crate::error::FrameworkError;
    use crate::gateway::Gateway;
    use crate::memory::{
        DynMemory, LongTermFactSummary, LongTermForgetResult, MemorizeResult, Memory,
        MemoryHitStore, MemoryRecallHit, MemoryStoreScope, StoredMessage, StoredRole,
    };
    use crate::providers::{Message, Provider, ProviderFactory, ProviderResponse, ToolDefinition};
    use crate::react::ReactLoop;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, default_factory,
    };

    use super::{
        AgentDirectory, AgentRuntime, AgentRuntimeConfig, RuntimeContext, ToolRuntime,
        format_recalled_memory, inject_caller_context,
    };

    #[derive(Default)]
    struct FakeMemory {
        appended: Mutex<Vec<(String, StoredRole, String, Option<String>)>>,
        recent_messages: Mutex<Vec<StoredMessage>>,
        recall_hits: Mutex<Vec<MemoryRecallHit>>,
    }

    impl FakeMemory {
        async fn appended(&self) -> Vec<(String, StoredRole, String, Option<String>)> {
            self.appended.lock().await.clone()
        }

        async fn set_recent_messages(&self, messages: Vec<StoredMessage>) {
            *self.recent_messages.lock().await = messages;
        }

        async fn set_recall_hits(&self, hits: Vec<MemoryRecallHit>) {
            *self.recall_hits.lock().await = hits;
        }
    }

    #[async_trait]
    impl Memory for FakeMemory {
        async fn append_message(
            &self,
            session_id: &str,
            role: StoredRole,
            content: &str,
            username: Option<&str>,
        ) -> Result<(), FrameworkError> {
            self.appended.lock().await.push((
                session_id.to_owned(),
                role,
                content.to_owned(),
                username.map(str::to_owned),
            ));
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
            Ok(self.recall_hits.lock().await.clone())
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
                similarity_threshold: 0.0,
                max_matches: 0,
                kind_filter: None,
                deleted_count: 0,
                matches: Vec::new(),
            })
        }

        async fn recent_messages(
            &self,
            _session_id: &str,
            _limit: usize,
        ) -> Result<Vec<StoredMessage>, FrameworkError> {
            Ok(self.recent_messages.lock().await.clone())
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
        reply: String,
        calls: AtomicUsize,
        system_prompts: Mutex<Vec<String>>,
        histories: Mutex<Vec<Vec<Message>>>,
    }

    impl RecordingProvider {
        fn new(reply: impl Into<String>) -> Self {
            Self {
                reply: reply.into(),
                calls: AtomicUsize::new(0),
                system_prompts: Mutex::new(Vec::new()),
                histories: Mutex::new(Vec::new()),
            }
        }

        async fn system_prompts(&self) -> Vec<String> {
            self.system_prompts.lock().await.clone()
        }

        async fn histories(&self) -> Vec<Vec<Message>> {
            self.histories.lock().await.clone()
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for RecordingProvider {
        async fn generate(
            &self,
            system_prompt: &str,
            history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.system_prompts
                .lock()
                .await
                .push(system_prompt.to_owned());
            self.histories.lock().await.push(history.to_vec());
            Ok(ProviderResponse {
                output_text: Some(self.reply.clone()),
                tool_calls: Vec::new(),
            })
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
            Err(FrameworkError::Tool(
                "agent invocation should not occur in this test".to_owned(),
            ))
        }

        async fn invoke_worker(
            &self,
            _request: crate::tools::WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool(
                "worker invocation should not occur in this test".to_owned(),
            ))
        }
    }

    struct QuietChannel;

    #[async_trait]
    impl Channel for QuietChannel {
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

    fn test_inbound(is_dm: bool, guild_id: Option<&str>) -> InboundMessage {
        InboundMessage {
            trace_id: "test-trace".to_owned(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "agent-1".to_owned(),
            session_key: "sess-1".to_owned(),
            source_message_id: Some("msg-1".to_owned()),
            channel_id: "chan-456".to_owned(),
            guild_id: guild_id.map(|s| s.to_owned()),
            is_dm,
            user_id: "user-123".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            invoke: false,
            content: "hello".to_owned(),
        }
    }

    fn test_runtime_config() -> AgentRuntimeConfig {
        let mut agent_config = AgentInnerConfig::default();
        agent_config.tools = agent_config.tools.with_disabled(&["cron"]);
        let tool_registry = default_factory()
            .build_registry(&agent_config.tools, &[])
            .expect("tool registry should build");
        AgentRuntimeConfig {
            agent_id: "agent-1".to_owned(),
            provider_key: "default".to_owned(),
            effective_execution: ExecutionDefaultsConfig {
                history_messages: 3,
                ..ExecutionDefaultsConfig::default()
            },
            owner_ids: vec!["user-123".to_owned()],
            agent_config,
            tool_registry,
            persona_root: PathBuf::from("/tmp/simpleclaw-agent-persona"),
            workspace_root: PathBuf::from("/tmp/simpleclaw-agent-test"),
            app_base_dir: PathBuf::from("/tmp/simpleclaw-agent-app"),
            system_prompt: "base prompt".to_owned(),
        }
    }

    fn test_gateway() -> Arc<Gateway> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, Arc::new(QuietChannel));
        Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        ))
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

    fn test_runtime_context(memory: DynMemory, react_loop: Arc<ReactLoop>) -> RuntimeContext {
        let directory = Arc::new(AgentDirectory::new(
            HashMap::from([("agent-1".to_owned(), test_runtime_config())]),
            HashMap::from([("agent-1".to_owned(), memory)]),
        ));
        let gateway = test_gateway();
        let (completion_tx, _completion_rx) = mpsc::channel(4);
        RuntimeContext {
            react_loop,
            gateway,
            agents: directory,
            tool_runtime: Arc::new(ToolRuntime {
                async_tool_runs: Arc::new(AsyncToolRunManager::new()),
                completion_tx,
            }),
        }
    }

    #[test]
    fn format_recalled_memory_caps_output_by_char_limit() {
        let hits = vec![
            MemoryRecallHit {
                store: MemoryHitStore::LongTerm,
                content: "Prefers concise responses".to_owned(),
                kind: Some("prefs".to_owned()),
                importance: Some(5),
                raw_similarity: 0.91,
                final_score: 0.88,
            },
            MemoryRecallHit {
                store: MemoryHitStore::LongTerm,
                content: "Asked about runtime memory config".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(3),
                raw_similarity: 0.89,
                final_score: 0.78,
            },
        ];

        let section = format_recalled_memory(&hits, 280);
        assert!(section.section.starts_with("# POTENTIALLY RELEVANT MEMORY"));
        assert!(section.section.contains("optional background context"));
        assert!(section.section.contains("1. [long-term/prefs]"));
        assert!(!section.section.contains("score="));
        assert!(
            section.section.contains("2."),
            "both items should fit without score metadata"
        );
        assert_eq!(section.short_hits, 0);
        assert_eq!(section.long_hits, 2);
    }

    #[test]
    fn inject_caller_context_adds_environment_and_speaker() {
        let dm = test_inbound(true, None);
        let output = inject_caller_context("base prompt", &dm);
        assert!(output.contains("# CURRENT CONTEXT"));
        assert!(output.contains("chat_type: dm"));
        assert!(output.contains("platform: discord"));
        assert!(output.contains("channel_id: chan-456"));
        assert!(output.contains("message_id: msg-1"));
        assert!(!output.contains("guild_id:"));
        assert!(output.contains("Speaker: **kaleb** (id: user-123)"));

        let guild = test_inbound(false, Some("guild-789"));
        let output = inject_caller_context("base prompt", &guild);
        assert!(output.contains("chat_type: group"));
        assert!(output.contains("platform: discord"));
        assert!(output.contains("channel_id: chan-456"));
        assert!(output.contains("guild_id: guild-789"));
        assert!(output.contains("message_id: msg-1"));
    }

    #[test]
    fn inject_caller_context_marks_scheduled_cron_trigger() {
        let mut inbound = test_inbound(false, None);
        inbound.user_id = "system".to_owned();
        inbound.username = "cron".to_owned();
        inbound.source_message_id = None;

        let output = inject_caller_context("base prompt", &inbound);

        assert!(output.contains("trigger: scheduled_cron"));
        assert!(output.contains("Speaker: **cron** (id: system)"));
    }

    #[tokio::test]
    async fn agent_runtime_persists_user_and_assistant_messages_and_seeds_history() {
        let memory_impl = Arc::new(FakeMemory::default());
        memory_impl
            .set_recent_messages(vec![StoredMessage {
                role: StoredRole::Assistant,
                content: "previous reply".to_owned(),
                username: None,
            }])
            .await;
        let provider_impl = Arc::new(RecordingProvider::new("final reply"));
        let memory: DynMemory = memory_impl.clone();
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let context = test_runtime_context(memory, test_react_loop(provider));
        let runtime = AgentRuntime::new(test_runtime_config());

        let outcome = runtime
            .run(
                &test_inbound(false, Some("guild-789")),
                "sess-1",
                &context,
                None,
            )
            .await
            .expect("runtime should succeed");

        assert_eq!(outcome.reply, "final reply");
        assert_eq!(provider_impl.call_count(), 1);

        let appended = memory_impl.appended().await;
        assert_eq!(appended.len(), 2);
        assert_eq!(appended[0].1, StoredRole::User);
        assert_eq!(appended[0].2, "hello");
        assert_eq!(appended[0].3.as_deref(), Some("kaleb"));
        assert_eq!(appended[1].1, StoredRole::Assistant);
        assert_eq!(appended[1].2, "final reply");

        let histories = provider_impl.histories().await;
        assert_eq!(histories.len(), 1);
        assert_eq!(histories[0].len(), 1);
        assert_eq!(histories[0][0].role, crate::providers::Role::Assistant);
        assert_eq!(histories[0][0].content, "previous reply");

        let prompts = provider_impl.system_prompts().await;
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("# CURRENT CONTEXT"));
        assert!(prompts[0].contains("guild_id: guild-789"));
    }

    #[tokio::test]
    async fn agent_runtime_skips_assistant_persist_for_no_reply_and_reports_memory_recall_hits() {
        let memory_impl = Arc::new(FakeMemory::default());
        memory_impl
            .set_recall_hits(vec![MemoryRecallHit {
                store: MemoryHitStore::LongTerm,
                content: "Prefers short answers".to_owned(),
                kind: Some("preferences".to_owned()),
                importance: Some(5),
                raw_similarity: 0.9,
                final_score: 0.85,
            }])
            .await;
        let provider_impl = Arc::new(RecordingProvider::new("NO_REPLY"));
        let memory: DynMemory = memory_impl.clone();
        let provider: Arc<dyn Provider> = provider_impl.clone();
        let mut config = test_runtime_config();
        config.effective_execution.memory_recall.enabled = true;
        let context = test_runtime_context(memory, test_react_loop(provider));
        let runtime = AgentRuntime::new(config);

        let outcome = runtime
            .run(&test_inbound(false, None), "sess-2", &context, None)
            .await
            .expect("runtime should succeed");

        assert_eq!(outcome.reply, "NO_REPLY");
        assert!(outcome.memory_recall_used);
        assert_eq!(outcome.memory_recall_short_hits, 0);
        assert_eq!(outcome.memory_recall_long_hits, 1);

        let appended = memory_impl.appended().await;
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].1, StoredRole::User);

        let prompts = provider_impl.system_prompts().await;
        assert!(prompts[0].contains("# POTENTIALLY RELEVANT MEMORY"));
        assert!(prompts[0].contains("Prefers short answers"));
    }
}
