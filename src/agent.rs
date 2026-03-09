use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channels::InboundMessage;
use crate::config::{AgentConfig, ExecutionConfig, ToolCallTransparency};
use crate::dispatch::ToolExecutionResult;
use crate::error::FrameworkError;
use crate::memory::{DynMemory, MemoryHitStore, MemoryPreinjectHit, StoredRole};
use crate::prompt::PromptAssembler;
use crate::providers::{Message, Role};
use crate::react::{ReactLoop, RunOutcome, RunParams};
use crate::telemetry::sanitize_preview;
use crate::tools::{CompletionRoute, ProcessManager};

/// Groups declarative parameters needed for an `AgentRuntime`.
#[derive(Debug, Clone)]
pub struct AgentRuntimeConfig {
    pub agent_id: String,
    pub provider_key: String,
    pub execution_config: ExecutionConfig,
    pub agent_config: AgentConfig,
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
    pub agents: Arc<AgentDirectory>,
    pub process_manager: Arc<ProcessManager>,
    pub completion_tx: mpsc::Sender<InboundMessage>,
    pub safe_error_reply: String,
    pub tool_call_transparency: ToolCallTransparency,
}

pub struct AgentRuntime {
    config: AgentRuntimeConfig,
}

impl AgentRuntime {
    pub fn new(config: AgentRuntimeConfig) -> Self {
        Self { config }
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
        let system_prompt = self
            .build_turn_system_prompt(&memory, memory_session_id, &inbound.content)
            .await;
        let system_prompt = inject_caller_context(&system_prompt, inbound);
        debug!(status = "history_loaded", "agent context");

        let effective_max_steps = self.config.execution_config.defaults.max_steps;

        let params = RunParams {
            provider_key: &self.config.provider_key,
            agent_config: &self.config.agent_config,
            system_prompt: &system_prompt,
            agent_id: &self.config.agent_id,
            session_id: memory_session_id,
            max_steps: effective_max_steps,
            memory: memory.clone(),
            sandbox: self.config.agent_config.sandbox.clone(),
            workspace_root: self.config.workspace_root.clone(),
            user_id: inbound.user_id.clone(),
            owner_ids: self.config.execution_config.owner_ids.clone(),
            process_manager: Arc::clone(&context.process_manager),
            completion_tx: Some(context.completion_tx.clone()),
            completion_route: Some(CompletionRoute {
                trace_id: inbound.trace_id.clone(),
                source_channel: inbound.source_channel,
                target_agent_id: self.config.agent_id.clone(),
                session_key: inbound.session_key.clone(),
                channel_id: inbound.channel_id.clone(),
                guild_id: inbound.guild_id.clone(),
                is_dm: inbound.is_dm,
            }),
        };

        let outcome = match context.react_loop.run(params, history).await {
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

        let tool_suffix = format_tool_history_suffix(&outcome.tool_calls);
        let stored_reply = if tool_suffix.is_empty() {
            outcome.reply.clone()
        } else {
            format!("{}{tool_suffix}", outcome.reply)
        };
        memory
            .append_message(
                memory_session_id,
                StoredRole::Assistant,
                &stored_reply,
                None,
            )
            .await?;
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
        let history_limit = self.config.execution_config.defaults.history_messages as usize;
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
    ) -> String {
        let config = self.config.execution_config.defaults.memory_preinject.normalized();
        if !config.enabled {
            return self.config.system_prompt.clone();
        }

        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return self.config.system_prompt.clone();
        }

        let hits = match memory
            .query_preinject_hits(session_id, trimmed_query, &config)
            .await
        {
            Ok(items) => items,
            Err(err) => {
                warn!(
                    status = "failed",
                    error_kind = "memory_preinject_query",
                    error = %err,
                    "memory preinject query"
                );
                return self.config.system_prompt.clone();
            }
        };

        if hits.is_empty() {
            debug!(status = "completed", "memory preinject");
            return self.config.system_prompt.clone();
        }

        debug!(status = "completed", "memory preinject");

        let section = format_preinjected_memory(&hits, config.max_chars as usize);
        if section.is_empty() {
            return self.config.system_prompt.clone();
        }

        format!("{}\n\n{}", self.config.system_prompt, section)
    }
}

fn format_preinjected_memory(hits: &[MemoryPreinjectHit], max_chars: usize) -> String {
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

fn memory_hit_label(hit: &MemoryPreinjectHit) -> String {
    match hit.store {
        MemoryHitStore::LongTerm => {
            format!("long-term/{}", hit.kind.as_deref().unwrap_or("general"))
        }
    }
}

fn inject_caller_context(base: &str, inbound: &InboundMessage) -> String {
    let environment = if inbound.is_dm {
        "Direct message".to_owned()
    } else {
        match &inbound.guild_id {
            Some(gid) => format!("Guild channel (guild: {}, channel: {})", gid, inbound.channel_id),
            None => format!("Group channel (channel: {})", inbound.channel_id),
        }
    };
    format!(
        "{base}\n\n# CURRENT CONTEXT\nEnvironment: {environment}\nSpeaker: **{}** (id: {})",
        inbound.username, inbound.user_id
    )
}

fn format_tool_history_suffix(tool_calls: &[ToolExecutionResult]) -> String {
    if tool_calls.is_empty() {
        return String::new();
    }
    let entries: Vec<String> = tool_calls
        .iter()
        .map(|call| {
            let args = sanitize_preview(&call.args_json, 40);
            let output = sanitize_preview(&call.output, 60);
            format!("{}({}) -> \"{}\"", call.name, args, output)
        })
        .collect();
    format!("\n\n---\n[Tools used: {}]", entries.join(", "))
}

pub(crate) fn load_system_prompt_for_workspace(workspace: &Path) -> Result<String, FrameworkError> {
    PromptAssembler::from_workspace(workspace)
}

#[cfg(test)]
mod tests {
    use crate::channels::InboundMessage;
    use crate::config::GatewayChannelKind;
    use crate::dispatch::ToolExecutionResult;
    use crate::memory::{MemoryHitStore, MemoryPreinjectHit};

    use super::{format_preinjected_memory, format_tool_history_suffix, inject_caller_context};

    fn test_inbound(is_dm: bool, guild_id: Option<&str>) -> InboundMessage {
        InboundMessage {
            trace_id: "test-trace".to_owned(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "agent-1".to_owned(),
            session_key: "sess-1".to_owned(),
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
    fn format_preinjected_memory_caps_output_by_char_limit() {
        let hits = vec![
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Prefers concise responses".to_owned(),
                kind: Some("prefs".to_owned()),
                importance: Some(5),
                raw_similarity: 0.91,
                final_score: 0.88,
            },
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Asked about runtime memory config".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(3),
                raw_similarity: 0.89,
                final_score: 0.78,
            },
        ];

        let section = format_preinjected_memory(&hits, 280);
        assert!(section.starts_with("# POTENTIALLY RELEVANT LONG-TERM MEMORY"));
        assert!(section.contains("optional background context"));
        assert!(section.contains("1. [long-term/prefs]"));
        assert!(!section.contains("score="));
        assert!(section.contains("2."), "both items should fit without score metadata");
    }

    #[test]
    fn inject_caller_context_adds_environment_and_speaker() {
        let dm = test_inbound(true, None);
        let output = inject_caller_context("base prompt", &dm);
        assert!(output.contains("# CURRENT CONTEXT"));
        assert!(output.contains("Environment: Direct message"));
        assert!(output.contains("Speaker: **kaleb** (id: user-123)"));

        let guild = test_inbound(false, Some("guild-789"));
        let output = inject_caller_context("base prompt", &guild);
        assert!(output.contains("Environment: Guild channel (guild: guild-789, channel: chan-456)"));
    }

    #[test]
    fn format_tool_history_suffix_empty_when_no_calls() {
        assert!(format_tool_history_suffix(&[]).is_empty());
    }

    #[test]
    fn format_tool_history_suffix_includes_tool_summary() {
        let calls = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            args_json: "{}".to_owned(),
            output: "2026-03-08T12:00Z".to_owned(),
            success: true,
            elapsed_ms: 10,
            tool_call_id: None,
            nested_tool_calls: vec![],
        }];
        let suffix = format_tool_history_suffix(&calls);
        assert!(suffix.contains("[Tools used:"));
        assert!(suffix.contains("clock("));
        assert!(suffix.contains("2026-03-08T12:00Z"));
    }
}
