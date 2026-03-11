use crate::dispatch::{DispatchAction, ToolDispatcher, ToolExecutionResult};
use crate::error::FrameworkError;
use crate::gateway::Gateway;
use crate::providers::ProviderFactory;
use crate::providers::{Message, Provider, ProviderResponse, Role, StreamEvent};
use crate::reply_policy::no_reply_prompt_instruction;
use crate::tools::ProcessManager;
use crate::tools::{AgentInvoker, AgentToolRegistry, CompletionRoute, ToolExecEnv};
use crate::{channels::InboundMessage, memory::DynMemory};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{Instrument, debug, error, info, info_span, warn};

#[cfg(test)]
const REDACTED: &str = "***REDACTED***";

pub struct ReactLoop {
    provider_factory: ProviderFactory,
    invoker: Arc<dyn AgentInvoker>,
}

pub struct RunParams<'a> {
    pub provider_key: &'a str,
    pub system_prompt: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub max_steps: u32,
    pub history_messages: usize,
    pub execution_env: BTreeMap<String, String>,
    pub memory: DynMemory,
    pub persona_root: std::path::PathBuf,
    pub workspace_root: std::path::PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub process_manager: Arc<ProcessManager>,
    pub tool_registry: AgentToolRegistry,
    pub gateway: Option<Arc<Gateway>>,
    pub completion_tx: Option<mpsc::Sender<InboundMessage>>,
    pub completion_route: Option<CompletionRoute>,
    pub source_message_id: Option<String>,
    pub on_text_delta: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub reply: String,
    pub tool_calls: Vec<ToolExecutionResult>,
    pub memory_recall_used: bool,
    pub memory_recall_short_hits: usize,
    pub memory_recall_long_hits: usize,
}

impl ReactLoop {
    pub fn new(provider_factory: ProviderFactory, invoker: Arc<dyn AgentInvoker>) -> Self {
        Self {
            provider_factory,
            invoker,
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
        let invoker = Arc::clone(&self.invoker);
        let tool_env = ToolExecEnv {
            agent_id: params.agent_id.to_owned(),
            memory: params.memory.clone(),
            history_messages: params.history_messages,
            env: params.execution_env.clone(),
            persona_root: params.persona_root.clone(),
            workspace_root: params.workspace_root.clone(),
            user_id: params.user_id.clone(),
            owner_ids: params.owner_ids.clone(),
            process_manager: Arc::clone(&params.process_manager),
            invoker,
            gateway: params.gateway.clone(),
            completion_tx: params.completion_tx.clone(),
            completion_route: params.completion_route.clone(),
        };
        let tool_registry = params.tool_registry.clone();
        run_loop(
            provider,
            dispatcher,
            params,
            &tool_env,
            &tool_registry,
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
    active_tools: &AgentToolRegistry,
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
        let response = match generate_provider_response(
            provider,
            &effective_system_prompt,
            history,
            &tool_specs,
            params.on_text_delta.as_deref(),
        )
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
                    memory_recall_short_hits: 0,
                    memory_recall_long_hits: 0,
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
        memory_recall_short_hits: 0,
        memory_recall_long_hits: 0,
    })
}

async fn generate_provider_response(
    provider: &dyn Provider,
    system_prompt: &str,
    history: &[Message],
    tool_specs: &[crate::providers::ToolDefinition],
    on_text_delta: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<ProviderResponse, FrameworkError> {
    let Some(on_text_delta) = on_text_delta else {
        return provider.generate(system_prompt, history, tool_specs).await;
    };

    let mut stream = provider
        .generate_stream(system_prompt, history, tool_specs)
        .await?;
    let mut output_text = String::new();
    let mut tool_calls = Vec::new();

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                output_text.push_str(&delta);
                on_text_delta(&output_text);
            }
            StreamEvent::ToolCallComplete(tool_call) => tool_calls.push(tool_call),
            StreamEvent::Done => break,
            StreamEvent::Error(message) => {
                return Err(FrameworkError::Provider(message));
            }
        }
    }

    Ok(ProviderResponse {
        output_text: (!output_text.is_empty()).then_some(output_text),
        tool_calls,
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
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio_stream::iter;

    use crate::config::ToolsConfig;
    use crate::providers::{ProviderStream, ToolCall, ToolDefinition};
    use crate::tools::default_factory;

    use super::*;

    struct StreamingTestProvider {
        response: ProviderResponse,
        stream_events: Vec<StreamEvent>,
    }

    #[async_trait]
    impl Provider for StreamingTestProvider {
        async fn generate(
            &self,
            _system_prompt: &str,
            _history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            Ok(self.response.clone())
        }

        async fn generate_stream(
            &self,
            _system_prompt: &str,
            _history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderStream, FrameworkError> {
            Ok(Box::pin(iter(self.stream_events.clone())))
        }
    }

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
            .build_registry(&tools_enabled(&["memory", "summon", "clock", "read"]), &[])
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
            .build_registry(&tools_enabled(&["memory"]), &[])
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

    #[tokio::test]
    async fn generate_provider_response_streams_accumulated_text_and_tool_calls() {
        let provider = StreamingTestProvider {
            response: ProviderResponse {
                output_text: Some("unused".to_owned()),
                tool_calls: Vec::new(),
            },
            stream_events: vec![
                StreamEvent::TextDelta("hel".to_owned()),
                StreamEvent::TextDelta("lo".to_owned()),
                StreamEvent::ToolCallComplete(ToolCall {
                    id: Some("call-1".to_owned()),
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }),
                StreamEvent::Done,
            ],
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let on_text_delta: Arc<dyn Fn(&str) + Send + Sync> = {
            let observed = Arc::clone(&observed);
            Arc::new(move |text| {
                observed
                    .lock()
                    .expect("test callback mutex should not be poisoned")
                    .push(text.to_owned());
            })
        };

        let response =
            generate_provider_response(&provider, "system", &[], &[], Some(on_text_delta.as_ref()))
                .await
                .expect("streaming response should succeed");

        assert_eq!(response.output_text.as_deref(), Some("hello"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            observed
                .lock()
                .expect("test callback mutex should not be poisoned")
                .clone(),
            vec!["hel".to_owned(), "hello".to_owned()]
        );
    }

    #[tokio::test]
    async fn generate_provider_response_returns_stream_errors() {
        let provider = StreamingTestProvider {
            response: ProviderResponse {
                output_text: None,
                tool_calls: Vec::new(),
            },
            stream_events: vec![StreamEvent::Error("boom".to_owned())],
        };

        let err = generate_provider_response(&provider, "system", &[], &[], Some(&|_| {}))
            .await
            .expect_err("streaming response should fail");

        assert!(matches!(err, FrameworkError::Provider(message) if message == "boom"));
    }
}
