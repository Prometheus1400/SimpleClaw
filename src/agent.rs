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
use crate::tools::{CompletionRoute, ProcessManager};

/// Groups declarative parameters needed for an `AgentRuntime`.
#[derive(Debug, Clone)]
pub struct AgentRuntimeConfig {
    pub agent_id: String,
    pub provider_key: String,
    pub effective_execution: ExecutionDefaultsConfig,
    pub owner_ids: Vec<String>,
    pub agent_config: AgentInnerConfig,
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
    pub process_manager: Arc<ProcessManager>,
    pub completion_tx: mpsc::Sender<InboundMessage>,
    pub safe_error_reply: String,
}

pub struct AgentRuntime {
    config: AgentRuntimeConfig,
}

impl AgentRuntime {
    pub fn new(config: AgentRuntimeConfig) -> Self {
        Self { config }
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
            context
        ),
        fields(
            trace_id = %inbound.trace_id,
            session_id = %memory_session_id,
            agent_id = %self.config.agent_id
        )
    )]
    pub async fn run(
        &self,
        inbound: &InboundMessage,
        memory_session_id: &str,
        context: &RuntimeContext,
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
            agent_config: &self.config.agent_config,
            system_prompt: &system_prompt,
            agent_id: &self.config.agent_id,
            session_id: memory_session_id,
            max_steps: effective_max_steps,
            history_messages: self.config.effective_execution.history_messages as usize,
            memory: memory.clone(),
            workspace_root: self.config.workspace_root.clone(),
            user_id: inbound.user_id.clone(),
            owner_ids: self.config.owner_ids.clone(),
            process_manager: Arc::clone(&context.process_manager),
            gateway: Some(Arc::clone(&context.gateway)),
            completion_tx: Some(context.completion_tx.clone()),
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
        outcome.memory_recall_used = prompt_build.memory_recall_hits > 0;
        outcome.memory_recall_hits = prompt_build.memory_recall_hits;

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

        let section = format_recalled_memory(&hits, config.max_chars as usize);
        if section.is_empty() {
            return PromptBuild::without_recall(self.config.system_prompt.clone());
        }

        PromptBuild {
            system_prompt: format!("{}\n\n{}", self.config.system_prompt, section),
            memory_recall_hits: count_formatted_recalled_hits(&section),
        }
    }
}

struct PromptBuild {
    system_prompt: String,
    memory_recall_hits: usize,
}

impl PromptBuild {
    fn without_recall(system_prompt: String) -> Self {
        Self {
            system_prompt,
            memory_recall_hits: 0,
        }
    }
}

fn count_formatted_recalled_hits(section: &str) -> usize {
    section
        .lines()
        .filter(|line| line.starts_with(char::is_numeric))
        .count()
}

fn format_recalled_memory(hits: &[MemoryRecallHit], max_chars: usize) -> String {
    if hits.is_empty() || max_chars == 0 {
        return String::new();
    }

    let base = "# POTENTIALLY RELEVANT LONG-TERM MEMORY\nUse this as optional background context. It may be stale or incomplete; prioritize the current user message and conversation.";
    let mut section = base.to_owned();
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
    }

    if section == base {
        String::new()
    } else {
        section
    }
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
        "{base}\n\n# CURRENT CONTEXT\nchat_type: {chat_type}\nplatform: {platform}\nchannel_id: {}{guild_line}{message_line}\nSpeaker: **{}** (id: {})",
        inbound.channel_id, inbound.username, inbound.user_id
    )
}

pub(crate) fn load_system_prompt_for_workspace(workspace: &Path) -> Result<String, FrameworkError> {
    PromptAssembler::from_workspace(workspace)
}

#[cfg(test)]
mod tests {
    use crate::channels::InboundMessage;
    use crate::config::GatewayChannelKind;
    use crate::memory::{MemoryHitStore, MemoryRecallHit};

    use super::{format_recalled_memory, inject_caller_context};

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
        assert!(section.starts_with("# POTENTIALLY RELEVANT LONG-TERM MEMORY"));
        assert!(section.contains("optional background context"));
        assert!(section.contains("1. [long-term/prefs]"));
        assert!(!section.contains("score="));
        assert!(
            section.contains("2."),
            "both items should fit without score metadata"
        );
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
}
