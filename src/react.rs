use crate::dispatch::{DispatchAction, ToolDispatcher};
use crate::error::FrameworkError;
use crate::providers::ProviderFactory;
use crate::providers::{Message, Provider, Role};
use crate::tools::skill::SkillFactory;
use crate::tools::{
    AgentInvokeRequest, AgentInvoker, CompletionRoute, ToolExecEnv, ToolFactory,
    WorkerInvokeRequest,
};
use crate::{agent::AgentDirectory, channels::InboundMessage, memory::DynMemory};
use crate::{config::AgentSandboxConfig, tools::ProcessManager};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span, warn};

#[cfg(test)]
const REDACTED: &str = "***REDACTED***";

pub struct ReactLoop {
    provider_factory: ProviderFactory,
    tool_factory: ToolFactory,
    skill_factory: SkillFactory,
}

pub struct RunParams<'a> {
    pub provider_key: &'a str,
    pub agent_config: &'a crate::config::AgentConfig,
    pub system_prompt: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub max_steps: u32,
    pub memory: DynMemory,
    pub sandbox: AgentSandboxConfig,
    pub workspace_root: std::path::PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub process_manager: Arc<ProcessManager>,
    pub react_loop: Arc<ReactLoop>,
    pub agents: Arc<AgentDirectory>,
    pub completion_tx: Option<mpsc::Sender<InboundMessage>>,
    pub completion_route: Option<CompletionRoute>,
}

struct ReactAgentInvoker {
    react_loop: Arc<ReactLoop>,
    agents: Arc<AgentDirectory>,
    process_manager: Arc<ProcessManager>,
}

#[async_trait::async_trait]
impl AgentInvoker for ReactAgentInvoker {
    async fn invoke_agent(&self, request: AgentInvokeRequest) -> Result<String, FrameworkError> {
        let target_config = self
            .agents
            .config(&request.target_agent_id)
            .ok_or_else(|| {
                FrameworkError::Tool(format!("unknown agent: {}", request.target_agent_id))
            })?;
        let memory = self
            .agents
            .memory(&request.target_agent_id)
            .cloned()
            .ok_or_else(|| {
                FrameworkError::Tool(format!("no memory for agent: {}", request.target_agent_id))
            })?;
        let effective_max_steps = target_config
            .max_steps
            .min(target_config.runtime_config.max_steps);
        let params = RunParams {
            provider_key: &target_config.provider_key,
            agent_config: &target_config.agent_config,
            system_prompt: &target_config.system_prompt,
            agent_id: &request.target_agent_id,
            session_id: &request.session_id,
            max_steps: effective_max_steps,
            memory,
            sandbox: target_config.agent_config.sandbox.clone(),
            workspace_root: target_config.workspace_root.clone(),
            user_id: request.user_id,
            owner_ids: target_config.runtime_config.owner_ids.clone(),
            process_manager: Arc::clone(&self.process_manager),
            react_loop: Arc::clone(&self.react_loop),
            agents: Arc::clone(&self.agents),
            completion_tx: None,
            completion_route: None,
        };
        self.react_loop
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
    }

    async fn invoke_worker(&self, request: WorkerInvokeRequest) -> Result<String, FrameworkError> {
        let current_config = self
            .agents
            .config(&request.current_agent_id)
            .ok_or_else(|| FrameworkError::Tool("current agent config unavailable".to_owned()))?;
        let memory = self
            .agents
            .memory(&request.current_agent_id)
            .cloned()
            .ok_or_else(|| FrameworkError::Tool("current agent memory unavailable".to_owned()))?;
        let mut worker_agent_config = current_config.agent_config.clone();
        worker_agent_config.tools.enabled_tools = worker_agent_config
            .tools
            .enabled_tools
            .iter()
            .filter(|name| !matches!(name.as_str(), "summon" | "task" | "memorize" | "forget"))
            .cloned()
            .collect();
        let params = RunParams {
            provider_key: &current_config.provider_key,
            agent_config: &worker_agent_config,
            system_prompt: "You are a task worker. Complete the assigned task and return a concise result.",
            agent_id: "task-worker",
            session_id: &request.session_id,
            max_steps: current_config
                .max_steps
                .min(current_config.runtime_config.max_steps),
            memory,
            sandbox: current_config.agent_config.sandbox.clone(),
            workspace_root: current_config.workspace_root.clone(),
            user_id: request.user_id,
            owner_ids: current_config.runtime_config.owner_ids.clone(),
            process_manager: Arc::clone(&self.process_manager),
            react_loop: Arc::clone(&self.react_loop),
            agents: Arc::clone(&self.agents),
            completion_tx: None,
            completion_route: None,
        };
        self.react_loop
            .run(params, vec![Message::text(Role::User, request.prompt)])
            .await
    }
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
    ) -> Result<String, FrameworkError> {
        let provider = self.provider_factory.get(params.provider_key)?;
        let supports_native = self
            .provider_factory
            .supports_native_tools(params.provider_key);
        let dispatcher = resolve_dispatcher(supports_native);
        let skills = self.skill_factory.tools_for_agent(params.agent_id);
        let invoker: Arc<dyn AgentInvoker> = Arc::new(ReactAgentInvoker {
            react_loop: Arc::clone(&params.react_loop),
            agents: Arc::clone(&params.agents),
            process_manager: Arc::clone(&params.process_manager),
        });
        let tool_env = ToolExecEnv {
            memory: params.memory.clone(),
            sandbox: params.sandbox.clone(),
            workspace_root: params.workspace_root.clone(),
            user_id: params.user_id.clone(),
            owner_ids: params.owner_ids.clone(),
            process_manager: Arc::clone(&params.process_manager),
            invoker,
            completion_tx: params.completion_tx.clone(),
            completion_route: params.completion_route.clone(),
        };
        let active_tools = self
            .tool_factory
            .resolve_active(&params.agent_config.tools.enabled_tools, skills)?;
        run_loop(
            provider,
            dispatcher.as_ref(),
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
) -> Result<String, FrameworkError> {
    let definitions = active_tools.definitions();
    let run_started = Instant::now();
    let extra_instructions = dispatcher.prompt_instructions(&definitions);
    let effective_system_prompt = if extra_instructions.is_empty() {
        params.system_prompt.to_owned()
    } else {
        format!("{}{extra_instructions}", params.system_prompt)
    };

    let tool_specs = if dispatcher.should_send_tool_specs() {
        definitions
    } else {
        vec![]
    };

    info!(status = "started", "react loop");

    for _ in 0..params.max_steps {
        let provider_started = Instant::now();
        let turn_span = info_span!("provider.turn");
        debug!(parent: &turn_span, status = "started", "provider turn");
        let response = match provider
            .generate(&effective_system_prompt, &history, &tool_specs)
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
                    .execute_tool_calls(&calls, active_tools, tool_env, params.session_id)
                    .instrument(turn_span.clone())
                    .await;
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
                return Ok(text);
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
    Ok("max_steps reached without final response".to_owned())
}

fn resolve_dispatcher(supports_native_tools: bool) -> Arc<dyn ToolDispatcher> {
    if supports_native_tools {
        Arc::new(crate::dispatch::NativeDispatcher)
    } else {
        Arc::new(crate::dispatch::XmlDispatcher)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn sanitize_log_preview(text: &str, max_chars: usize) -> String {
    crate::telemetry::sanitize_preview(text, max_chars)
}

#[cfg(test)]
mod tests {
    use crate::tools::default_factory;

    use super::*;

    #[test]
    fn tool_definitions_only_include_enabled_tools() {
        let factory = default_factory();
        let active_tools = factory
            .resolve_active(
                &[
                    "memory".to_owned(),
                    "summon".to_owned(),
                    "clock".to_owned(),
                    "read".to_owned(),
                ],
                &[],
            )
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
            .resolve_active(&["memory".to_owned()], &[])
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
