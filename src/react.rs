use crate::dispatch::{DispatchAction, ToolDispatcher, ToolExecutionResult};
use crate::error::FrameworkError;
use crate::gateway::Gateway;
use crate::providers::ProviderFactory;
use crate::providers::{Message, Provider, Role};
use crate::reply_policy::no_reply_prompt_instruction;
use crate::tools::ProcessManager;
use crate::tools::skill::SkillFactory;
use crate::tools::{AgentInvoker, CompletionRoute, ToolExecEnv, ToolFactory};
use crate::{channels::InboundMessage, memory::DynMemory};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span, warn};

#[cfg(test)]
const REDACTED: &str = "***REDACTED***";

pub struct ReactLoop {
    provider_factory: ProviderFactory,
    tool_factory: ToolFactory,
    skill_factory: SkillFactory,
    invoker: OnceLock<Arc<dyn AgentInvoker>>,
}

pub struct RunParams<'a> {
    pub provider_key: &'a str,
    pub agent_config: &'a crate::config::AgentInnerConfig,
    pub system_prompt: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub max_steps: u32,
    pub history_messages: usize,
    pub memory: DynMemory,
    pub workspace_root: std::path::PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub process_manager: Arc<ProcessManager>,
    pub gateway: Option<Arc<Gateway>>,
    pub completion_tx: Option<mpsc::Sender<InboundMessage>>,
    pub completion_route: Option<CompletionRoute>,
    pub source_message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub reply: String,
    pub tool_calls: Vec<ToolExecutionResult>,
    pub memory_recall_used: bool,
    pub memory_recall_hits: usize,
}

impl ReactLoop {
    pub fn new(
        provider_factory: ProviderFactory,
        tool_factory: ToolFactory,
        skill_factory: SkillFactory,
    ) -> Self {
        Self {
            provider_factory,
            tool_factory,
            skill_factory,
            invoker: OnceLock::new(),
        }
    }

    /// Inject the agent invoker. Must be called exactly once after construction
    /// and before the first call to [`run`](Self::run).
    pub fn set_invoker(&self, invoker: Arc<dyn AgentInvoker>) {
        if self.invoker.set(invoker).is_err() {
            panic!("ReactLoop::set_invoker called more than once");
        }
    }

    #[tracing::instrument(
        name = "react.loop",
        skip(self, params, history),
        fields(session_id = %params.session_id, agent_id = %params.agent_id)
    )]
    pub async fn run(
        &self,
        params: RunParams<'_>,
        mut history: Vec<Message>,
    ) -> Result<RunOutcome, FrameworkError> {
        let provider = self.provider_factory.get(params.provider_key)?;
        let supports_native = self
            .provider_factory
            .supports_native_tools(params.provider_key);
        let dispatcher = resolve_dispatcher(supports_native);
        let skills = self.skill_factory.tools_for_agent(params.agent_id);
        let invoker = Arc::clone(self.invoker.get().expect("invoker not initialized"));
        let tool_env = ToolExecEnv {
            agent_id: params.agent_id.to_owned(),
            memory: params.memory.clone(),
            history_messages: params.history_messages,
            workspace_root: params.workspace_root.clone(),
            user_id: params.user_id.clone(),
            owner_ids: params.owner_ids.clone(),
            process_manager: Arc::clone(&params.process_manager),
            invoker,
            gateway: params.gateway.clone(),
            completion_tx: params.completion_tx.clone(),
            completion_route: params.completion_route.clone(),
        };
        let active_tools = self
            .tool_factory
            .resolve_active(&params.agent_config.tools, skills)?;
        run_loop(
            provider,
            dispatcher,
            params,
            &tool_env,
            &active_tools,
            &mut history,
        )
        .await
    }
}

async fn run_loop(
    provider: &dyn Provider,
    dispatcher: &dyn ToolDispatcher,
    params: RunParams<'_>,
    tool_env: &ToolExecEnv,
    active_tools: &crate::tools::ActiveTools,
    history: &mut Vec<Message>,
) -> Result<RunOutcome, FrameworkError> {
    let turn_tools = if params.source_message_id.is_some() {
        active_tools.clone()
    } else {
        active_tools.without("react")
    };
    let definitions = turn_tools.definitions();
    let run_started = Instant::now();
    let extra_instructions = dispatcher.prompt_instructions(&definitions);
    let no_reply_instructions = no_reply_prompt_instruction();
    let effective_system_prompt = if extra_instructions.is_empty() {
        format!("{}{}", params.system_prompt, no_reply_instructions)
    } else {
        format!(
            "{}{}{}",
            params.system_prompt, extra_instructions, no_reply_instructions
        )
    };

    let tool_specs = if dispatcher.should_send_tool_specs() {
        definitions
    } else {
        vec![]
    };

    info!(status = "started", "react loop");
    let mut executed_tool_calls: Vec<ToolExecutionResult> = Vec::new();

    for _ in 0..params.max_steps {
        let provider_started = Instant::now();
        let turn_span = info_span!("provider.turn");
        debug!(parent: &turn_span, status = "started", "provider turn");
        let response = match provider
            .generate(&effective_system_prompt, history, &tool_specs)
            .instrument(turn_span.clone())
            .await
        {
            Ok(response) => response,
            Err(err) => {
                error!(
                    parent: &turn_span,
                    status = "failed",
                    error_kind = "provider_generate",
                    elapsed_ms = provider_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider turn"
                );
                return Err(err);
            }
        };
        debug!(
            parent: &turn_span,
            status = "completed",
            elapsed_ms = provider_started.elapsed().as_millis() as u64,
            "provider turn"
        );

        match dispatcher.parse_response(&response) {
            DispatchAction::ToolCalls(calls) => {
                let results = dispatcher
                    .execute_tool_calls(&calls, &turn_tools, tool_env, params.session_id)
                    .instrument(turn_span.clone())
                    .await;
                executed_tool_calls.extend(results.iter().cloned());
                let messages = dispatcher.format_for_history(&calls, &results);
                history.extend(messages);
                continue;
            }
            DispatchAction::FinalResponse(text) => {
                history.push(Message::text(Role::Assistant, text.clone()));
                info!(
                    status = "completed",
                    elapsed_ms = run_started.elapsed().as_millis() as u64,
                    "react loop"
                );
                return Ok(RunOutcome {
                    reply: text,
                    tool_calls: executed_tool_calls,
                    memory_recall_used: false,
                    memory_recall_hits: 0,
                });
            }
            DispatchAction::Empty => {
                warn!(parent: &turn_span, status = "empty", "provider response");
                continue;
            }
        }
    }

    warn!(
        status = "max_steps_reached",
        elapsed_ms = run_started.elapsed().as_millis() as u64,
        "react loop"
    );
    Ok(RunOutcome {
        reply: "max_steps reached without final response".to_owned(),
        tool_calls: executed_tool_calls,
        memory_recall_used: false,
        memory_recall_hits: 0,
    })
}

fn resolve_dispatcher(supports_native_tools: bool) -> &'static dyn ToolDispatcher {
    static NATIVE: crate::dispatch::NativeDispatcher = crate::dispatch::NativeDispatcher;
    static XML: crate::dispatch::XmlDispatcher = crate::dispatch::XmlDispatcher;
    if supports_native_tools { &NATIVE } else { &XML }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn sanitize_log_preview(text: &str, max_chars: usize) -> String {
    crate::telemetry::sanitize_preview(text, max_chars)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::config::ToolsConfig;
    use crate::tools::default_factory;

    use super::*;

    fn tools_enabled(allowed: &[&str]) -> ToolsConfig {
        const ALL: &[&str] = &[
            "memory",
            "memorize",
            "forget",
            "summon",
            "task",
            "web_search",
            "clock",
            "cron",
            "react",
            "web_fetch",
            "read",
            "edit",
            "exec",
            "process",
            "skills",
        ];
        let allowed: HashSet<&str> = allowed.iter().copied().collect();
        let disable: Vec<&str> = ALL
            .iter()
            .copied()
            .filter(|name| !allowed.contains(name))
            .collect();
        ToolsConfig::default().with_disabled(&disable)
    }

    #[test]
    fn tool_definitions_only_include_enabled_tools() {
        let factory = default_factory();
        let active_tools = factory
            .resolve_active(&tools_enabled(&["memory", "summon", "clock", "read"]), &[])
            .expect("enabled tools should resolve");

        let names: Vec<String> = active_tools
            .definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();

        assert_eq!(
            names,
            vec![
                "memory".to_owned(),
                "summon".to_owned(),
                "clock".to_owned(),
                "read".to_owned()
            ]
        );
    }

    #[test]
    fn tool_status_reports_unknown_tool_name() {
        let factory = default_factory();
        let active_tools = factory
            .resolve_active(&tools_enabled(&["memory"]), &[])
            .expect("enabled tools should resolve");

        assert!(active_tools.get("memory").is_some());
        assert!(active_tools.get("not_a_tool").is_none());
    }

    #[test]
    fn sanitize_log_preview_redacts_json_secret_keys() {
        let preview = sanitize_log_preview(
            r#"{"query":"ping","api_key":"abc123","nested":{"refresh_token":"xyz"}}"#,
            512,
        );
        assert!(preview.contains("\"query\":\"ping\""));
        assert!(preview.contains(REDACTED));
        assert!(!preview.contains("abc123"));
        assert!(!preview.contains("xyz"));
    }

    #[test]
    fn sanitize_log_preview_redacts_plaintext_secret_patterns() {
        let preview = sanitize_log_preview(
            "token=abc123 authorization:secret-value Authorization:Bearer super-secret",
            512,
        );
        assert!(!preview.contains("abc123"));
        assert!(!preview.contains("secret-value"));
        assert!(!preview.contains("super-secret"));
        assert!(preview.contains(REDACTED));
    }

    #[test]
    fn sanitize_log_preview_truncates_long_output() {
        let preview = sanitize_log_preview("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(preview, "abcdefghij...[truncated]");
    }

    #[test]
    fn sanitize_log_preview_flattens_newlines() {
        let preview = sanitize_log_preview("first line\nsecond\tline\r\nthird", 512);
        assert_eq!(preview, "first line second line third");
    }
}
