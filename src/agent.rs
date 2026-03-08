use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channels::InboundMessage;
use crate::config::{AgentConfig, RuntimeConfig};
use crate::error::FrameworkError;
use crate::memory::{DynMemory, MemoryHitStore, MemoryPreinjectHit, StoredRole};
use crate::prompt::PromptAssembler;
use crate::providers::{Message, Role};
use crate::react::{ReactLoop, RunParams};
use crate::tools::{CompletionRoute, ProcessManager};

/// Groups declarative parameters needed for an `AgentRuntime`.
#[derive(Debug, Clone)]
pub struct AgentRuntimeConfig {
    pub agent_id: String,
    pub provider_key: String,
    pub runtime_config: RuntimeConfig,
    pub agent_config: AgentConfig,
    pub workspace_root: PathBuf,
    #[allow(dead_code)]
    pub app_base_dir: PathBuf,
    pub system_prompt: String,
    pub max_steps: u32,
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
    ) -> Result<String, FrameworkError> {
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

        let display_identity = format!("{} (id:{})", inbound.username, inbound.user_id);
        memory
            .append_message(
                memory_session_id,
                StoredRole::User,
                &inbound.content,
                Some(&display_identity),
            )
            .await?;

        let history = self.seeded_history(&memory, memory_session_id).await?;
        let system_prompt = self
            .build_turn_system_prompt(&memory, memory_session_id, &inbound.content)
            .await;
        let system_prompt =
            inject_caller_context(&system_prompt, &inbound.user_id, &inbound.username);
        debug!(status = "history_loaded", "agent context");

        let effective_max_steps = self
            .config
            .max_steps
            .min(self.config.runtime_config.max_steps);

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
            owner_ids: self.config.runtime_config.owner_ids.clone(),
            process_manager: Arc::clone(&context.process_manager),
            react_loop: Arc::clone(&context.react_loop),
            agents: Arc::clone(&context.agents),
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

        let reply = match context.react_loop.run(params, history).await {
            Ok(reply) => reply,
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

        memory
            .append_message(memory_session_id, StoredRole::Assistant, &reply, None)
            .await?;
        info!(
            status = "completed",
            elapsed_ms = execution_started.elapsed().as_millis() as u64,
            "agent execution"
        );
        Ok(reply)
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
        let display_identity = format!("{} (id:{})", inbound.username, inbound.user_id);
        memory
            .append_message(
                memory_session_id,
                StoredRole::User,
                &inbound.content,
                Some(&display_identity),
            )
            .await
    }

    async fn seeded_history(
        &self,
        memory: &DynMemory,
        session_id: &str,
    ) -> Result<Vec<Message>, FrameworkError> {
        let history_limit = self.config.runtime_config.history_messages as usize;
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
        let config = self.config.runtime_config.memory_preinject.normalized();
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
        let score_details = match hit.importance {
            Some(importance) => format!(
                "score={:.2} raw={:.2} imp={importance}",
                hit.final_score, hit.raw_similarity
            ),
            None => format!("score={:.2} raw={:.2}", hit.final_score, hit.raw_similarity),
        };
        let line = format!(
            "\n{}. [{} {}] {}",
            index + 1,
            memory_hit_label(hit),
            score_details,
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

fn inject_caller_context(base: &str, user_id: &str, username: &str) -> String {
    format!(
        "{base}\n\n# CURRENT SPEAKER\nThe person speaking to you right now is **{username}** (id: {user_id}). Follow instructions from the current speaker for this turn."
    )
}

pub(crate) fn load_system_prompt_for_workspace(workspace: &Path) -> Result<String, FrameworkError> {
    PromptAssembler::from_workspace(workspace)
}

#[cfg(test)]
mod tests {
    use crate::memory::{MemoryHitStore, MemoryPreinjectHit};

    use super::{format_preinjected_memory, inject_caller_context};

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

        let section = format_preinjected_memory(&hits, 260);
        assert!(section.starts_with("# POTENTIALLY RELEVANT LONG-TERM MEMORY"));
        assert!(section.contains("optional background context"));
        assert!(section.contains("1. [long-term/prefs score=0.88 raw=0.91 imp=5]"));
        assert!(!section.contains("2."));
    }

    #[test]
    fn inject_caller_context_adds_speaker_identity() {
        let output = inject_caller_context("base prompt", "user-123", "kaleb");
        assert!(output.contains("# CURRENT SPEAKER"));
        assert!(output.contains("id: user-123"));
        assert!(output.contains("Follow instructions from the current speaker"));
    }
}
