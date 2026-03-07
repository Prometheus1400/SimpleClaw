use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::config::{AgentConfig, ProviderKind, RuntimeConfig, ToolConfig};
use crate::dispatch::{NativeDispatcher, ToolDispatcher, XmlDispatcher};
use crate::error::FrameworkError;
use crate::memory::{MemoryHitStore, MemoryPreinjectHit, MemoryStore};
use crate::prompt::PromptAssembler;
use crate::provider::{Message, Provider, Role};
use crate::react;
use crate::tools::skill::{SkillToolLoadStats, load_skill_tools};
use crate::tools::{
    ProcessManager, SummonService, TaskService, ToolCtx, ToolRegistry, default_registry,
};

pub struct AgentRuntime {
    agent_id: String,
    runtime_config: RuntimeConfig,
    agent_config: AgentConfig,
    provider: Arc<dyn Provider>,
    dispatcher: Arc<dyn ToolDispatcher>,
    memory: MemoryStore,
    tool_registry: Arc<ToolRegistry>,
    skill_tool_names: Vec<String>,
    process_manager: Arc<ProcessManager>,
    summon_agents: HashMap<String, PathBuf>,
    summon_memories: HashMap<String, MemoryStore>,
    workspace_root: PathBuf,
    app_base_dir: PathBuf,
    system_prompt: String,
    max_steps: u32,
}

impl AgentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_id: String,
        runtime_config: RuntimeConfig,
        agent_config: AgentConfig,
        provider: Arc<dyn Provider>,
        provider_kind: ProviderKind,
        memory: MemoryStore,
        summon_agents: HashMap<String, PathBuf>,
        summon_memories: HashMap<String, MemoryStore>,
        workspace_root: PathBuf,
        app_base_dir: PathBuf,
        system_prompt: String,
        tool_registry: Arc<ToolRegistry>,
        skill_tool_names: Vec<String>,
        max_steps: u32,
    ) -> Self {
        let dispatcher: Arc<dyn ToolDispatcher> = if provider_kind.supports_native_tools() {
            Arc::new(NativeDispatcher)
        } else {
            Arc::new(XmlDispatcher)
        };
        Self {
            agent_id,
            runtime_config,
            agent_config,
            provider,
            dispatcher,
            memory,
            tool_registry,
            skill_tool_names,
            process_manager: Arc::new(ProcessManager::new()),
            summon_agents,
            summon_memories,
            workspace_root,
            app_base_dir,
            system_prompt,
            max_steps,
        }
    }

    pub async fn run(
        &self,
        inbound: &crate::channel::InboundMessage,
        memory_session_id: &str,
    ) -> Result<String, FrameworkError> {
        let execution_started = Instant::now();
        info!(
            agent_id = %self.agent_id,
            session_id = %memory_session_id,
            channel_id = %inbound.channel_id,
            user_id = %inbound.user_id,
            max_steps = self.max_steps.min(self.runtime_config.max_steps),
            "agent execution started"
        );
        let display_identity = format!("{} (id:{})", inbound.username, inbound.user_id);
        self.memory
            .append_message(
                memory_session_id,
                "user",
                &inbound.content,
                Some(&display_identity),
            )
            .await?;
        let history = self.seeded_history(memory_session_id).await?;
        let system_prompt = self
            .build_turn_system_prompt(memory_session_id, &inbound.content)
            .await;
        let system_prompt =
            inject_caller_context(&system_prompt, &inbound.user_id, &inbound.username);
        debug!(
            agent_id = %self.agent_id,
            session_id = %memory_session_id,
            history_len = history.len(),
            "loaded seeded history"
        );

        let summon_service: Arc<dyn SummonService> = Arc::new(RuntimeSummonService {
            provider: Arc::clone(&self.provider),
            dispatcher: Arc::clone(&self.dispatcher),
            process_manager: Arc::clone(&self.process_manager),
            summon_agents: self.summon_agents.clone(),
            summon_memories: self.summon_memories.clone(),
            app_base_dir: self.app_base_dir.clone(),
            max_steps: self.max_steps.min(self.runtime_config.max_steps),
        });
        let effective_tools =
            with_auto_enabled_skill_tools(&self.agent_config.tools, &self.skill_tool_names);
        let active_tools = self.tool_registry.resolve_active(&effective_tools)?;
        let worker_enabled_tools = active_tools
            .names()
            .into_iter()
            .filter(|name| !matches!(*name, "summon" | "task" | "memorize" | "forget"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let task_service: Arc<dyn TaskService> = Arc::new(RuntimeTaskService {
            provider: Arc::clone(&self.provider),
            dispatcher: Arc::clone(&self.dispatcher),
            tool_registry: Arc::clone(&self.tool_registry),
            process_manager: Arc::clone(&self.process_manager),
            memory: self.memory.clone(),
            enabled_tools: worker_enabled_tools,
            sandbox: self.agent_config.sandbox,
            workspace_root: self.workspace_root.clone(),
            user_id: inbound.user_id.clone(),
            owner_ids: self.runtime_config.owner_ids.clone(),
            exec_container: self.runtime_config.exec_container.clone(),
            max_steps: self.max_steps.min(self.runtime_config.max_steps),
        });

        let tool_ctx = ToolCtx {
            memory: self.memory.clone(),
            sandbox: self.agent_config.sandbox,
            workspace_root: self.workspace_root.clone(),
            user_id: inbound.user_id.clone(),
            owner_ids: self.runtime_config.owner_ids.clone(),
            exec_container: self.runtime_config.exec_container.clone(),
            process_manager: Arc::clone(&self.process_manager),
            summon_service: Some(summon_service),
            task_service: Some(task_service),
        };

        let reply = match react::run_loop(
            self.provider.as_ref(),
            self.dispatcher.as_ref(),
            &tool_ctx,
            &active_tools,
            &system_prompt,
            &self.agent_id,
            memory_session_id,
            history,
            self.max_steps.min(self.runtime_config.max_steps),
        )
        .await
        {
            Ok(reply) => reply,
            Err(err) => {
                error!(
                    agent_id = %self.agent_id,
                    session_id = %memory_session_id,
                    elapsed_ms = execution_started.elapsed().as_millis() as u64,
                    error = %err,
                    "agent execution failed"
                );
                return Err(err);
            }
        };

        self.memory
            .append_message(memory_session_id, "assistant", &reply, None)
            .await?;
        info!(
            agent_id = %self.agent_id,
            session_id = %memory_session_id,
            elapsed_ms = execution_started.elapsed().as_millis() as u64,
            output_preview = %react::sanitize_log_preview(&reply, 120),
            "agent execution completed"
        );
        Ok(reply)
    }

    pub async fn record_context(
        &self,
        inbound: &crate::channel::InboundMessage,
        memory_session_id: &str,
    ) -> Result<(), FrameworkError> {
        let display_identity = format!("{} (id:{})", inbound.username, inbound.user_id);
        self.memory
            .append_message(
                memory_session_id,
                "user",
                &inbound.content,
                Some(&display_identity),
            )
            .await
    }

    async fn seeded_history(&self, session_id: &str) -> Result<Vec<Message>, FrameworkError> {
        let history_limit = self.runtime_config.history_messages as usize;
        let stored = self
            .memory
            .recent_messages(session_id, history_limit)
            .await?;
        let mut history = Vec::with_capacity(stored.len());
        for item in stored {
            let role = match item.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
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

    async fn build_turn_system_prompt(&self, session_id: &str, query: &str) -> String {
        let config = self.runtime_config.memory_preinject.normalized();
        if !config.enabled {
            return self.system_prompt.clone();
        }

        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return self.system_prompt.clone();
        }

        let hits = match self
            .memory
            .query_preinject_hits(session_id, trimmed_query, &config)
            .await
        {
            Ok(items) => items,
            Err(err) => {
                warn!(
                    agent_id = %self.agent_id,
                    session_id = %session_id,
                    error = %err,
                    "long-term memory pre-injection query failed; continuing without injected memory"
                );
                return self.system_prompt.clone();
            }
        };

        if hits.is_empty() {
            debug!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                query_preview = %react::sanitize_log_preview(trimmed_query, 120),
                min_score = config.min_score,
                top_k = config.top_k,
                "long-term memory pre-injection selected no hits"
            );
            return self.system_prompt.clone();
        }

        debug!(
            agent_id = %self.agent_id,
            session_id = %session_id,
            query_preview = %react::sanitize_log_preview(trimmed_query, 120),
            selected_hits = hits.len(),
            min_score = config.min_score,
            top_k = config.top_k,
            max_chars = config.max_chars,
            "long-term memory pre-injection selected hits"
        );
        for hit in &hits {
            debug!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                label = %memory_hit_label(hit),
                score = hit.final_score,
                raw_similarity = hit.raw_similarity,
                content_preview = %react::sanitize_log_preview(hit.content.trim(), 160),
                "long-term memory pre-injection hit"
            );
        }

        let section = format_preinjected_memory(&hits, config.max_chars as usize);
        if section.is_empty() {
            return self.system_prompt.clone();
        }

        format!("{}\n\n{}", self.system_prompt, section)
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

struct RuntimeSummonService {
    provider: Arc<dyn Provider>,
    dispatcher: Arc<dyn ToolDispatcher>,
    process_manager: Arc<ProcessManager>,
    summon_agents: HashMap<String, PathBuf>,
    summon_memories: HashMap<String, MemoryStore>,
    app_base_dir: PathBuf,
    max_steps: u32,
}

struct RuntimeTaskService {
    provider: Arc<dyn Provider>,
    dispatcher: Arc<dyn ToolDispatcher>,
    tool_registry: Arc<ToolRegistry>,
    process_manager: Arc<ProcessManager>,
    memory: MemoryStore,
    enabled_tools: Vec<String>,
    sandbox: crate::config::SandboxMode,
    workspace_root: PathBuf,
    user_id: String,
    owner_ids: Vec<String>,
    exec_container: crate::config::ExecContainerConfig,
    max_steps: u32,
}

#[async_trait]
impl SummonService for RuntimeSummonService {
    async fn summon(
        &self,
        target_agent: &str,
        summary: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let workspace = self.summon_agents.get(target_agent).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown summon target: {target_agent}"))
        })?;
        let target_memory = self.summon_memories.get(target_agent).ok_or_else(|| {
            FrameworkError::Tool(format!(
                "missing memory store for summon target: {target_agent}"
            ))
        })?;

        let target_agent_config = load_agent_config(workspace)?;
        let system_prompt = load_system_prompt_for_workspace(workspace)?;
        let target_tooling = build_tool_registry_for_agent(
            target_agent,
            &target_agent_config,
            workspace,
            &self.app_base_dir,
        )?;
        let effective_target_tools = with_auto_enabled_skill_tools(
            &target_agent_config.tools,
            &target_tooling.skill_tool_names,
        );
        let active_tools = target_tooling
            .tool_registry
            .resolve_active(&effective_target_tools)?;
        let handoff = if summary.trim().is_empty() {
            format!(
                "You were summoned as agent `{target_agent}`. Continue from session context and produce a final answer."
            )
        } else {
            format!(
                "You were summoned as agent `{target_agent}` with handoff summary:\n{}",
                summary
            )
        };

        let tool_ctx = ToolCtx {
            memory: target_memory.clone(),
            sandbox: target_agent_config.sandbox,
            workspace_root: workspace.clone(),
            user_id: String::new(),
            owner_ids: Vec::new(),
            exec_container: crate::config::ExecContainerConfig::default(),
            process_manager: Arc::clone(&self.process_manager),
            summon_service: None,
            task_service: None,
        };

        let output = react::run_loop(
            self.provider.as_ref(),
            self.dispatcher.as_ref(),
            &tool_ctx,
            &active_tools,
            &system_prompt,
            target_agent,
            session_id,
            vec![Message::text(Role::User, handoff)],
            self.max_steps,
        )
        .await?;

        Ok(output)
    }
}

#[async_trait]
impl TaskService for RuntimeTaskService {
    async fn run_task(&self, prompt: &str, session_id: &str) -> Result<String, FrameworkError> {
        let active_tools = self
            .tool_registry
            .resolve_active(&crate::config::ToolConfig {
                enabled_tools: self.enabled_tools.clone(),
            })?;

        let tool_ctx = ToolCtx {
            memory: self.memory.clone(),
            sandbox: self.sandbox,
            workspace_root: self.workspace_root.clone(),
            user_id: self.user_id.clone(),
            owner_ids: self.owner_ids.clone(),
            exec_container: self.exec_container.clone(),
            process_manager: Arc::clone(&self.process_manager),
            summon_service: None,
            task_service: None,
        };

        react::run_loop(
            self.provider.as_ref(),
            self.dispatcher.as_ref(),
            &tool_ctx,
            &active_tools,
            "You are a task worker. Complete the assigned task and return a concise result.",
            "task-worker",
            session_id,
            vec![Message::text(Role::User, prompt.to_owned())],
            self.max_steps,
        )
        .await
    }
}

fn load_agent_config(workspace: &Path) -> Result<AgentConfig, FrameworkError> {
    let agent_path = workspace.join("agent.yaml");
    if !agent_path.exists() {
        return Ok(AgentConfig::default());
    }

    let content = fs::read_to_string(agent_path)?;
    Ok(serde_yaml::from_str::<AgentConfig>(&content)?)
}

pub(crate) fn load_agent_config_for_workspace(
    workspace: &Path,
) -> Result<AgentConfig, FrameworkError> {
    load_agent_config(workspace)
}

pub(crate) fn load_system_prompt_for_workspace(workspace: &Path) -> Result<String, FrameworkError> {
    PromptAssembler::from_workspace(workspace)
}

pub(crate) struct AgentTooling {
    pub tool_registry: Arc<ToolRegistry>,
    pub skill_tool_names: Vec<String>,
    pub skill_stats: SkillToolLoadStats,
}

pub(crate) fn build_tool_registry_for_agent(
    agent_id: &str,
    agent_config: &AgentConfig,
    agent_workspace: &Path,
    app_base_dir: &Path,
) -> Result<AgentTooling, FrameworkError> {
    let mut registry = default_registry();
    let loaded_skills = load_skill_tools(
        agent_id,
        &agent_config.skills,
        agent_workspace,
        app_base_dir,
    )?;
    for skill_tool in loaded_skills.tools {
        registry.register(skill_tool);
    }

    Ok(AgentTooling {
        tool_registry: Arc::new(registry),
        skill_tool_names: loaded_skills.tool_names,
        skill_stats: loaded_skills.stats,
    })
}

fn with_auto_enabled_skill_tools(
    base_tools: &ToolConfig,
    skill_tool_names: &[String],
) -> ToolConfig {
    let mut enabled_tools = base_tools.enabled_tools.clone();
    let mut seen = enabled_tools.iter().cloned().collect::<HashSet<String>>();
    for name in skill_tool_names {
        if seen.insert(name.clone()) {
            enabled_tools.push(name.clone());
        }
    }

    ToolConfig { enabled_tools }
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
