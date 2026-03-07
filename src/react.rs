use crate::dispatch::{DispatchAction, ToolDispatcher};
use crate::error::FrameworkError;
use crate::provider::{Message, Provider, Role};
use crate::tools::{ActiveTools, ToolCtx};
use std::time::Instant;
use tracing::{Instrument, debug, error, info, info_span, warn};

const FINAL_OUTPUT_PREVIEW_CHARS: usize = 120;
#[cfg(test)]
const REDACTED: &str = "***REDACTED***";

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "react.loop",
    skip(provider, dispatcher, tool_ctx, active_tools, system_prompt, history),
    fields(session_id = %session_id, agent_id = %agent_id, max_steps)
)]
pub async fn run_loop(
    provider: &dyn Provider,
    dispatcher: &dyn ToolDispatcher,
    tool_ctx: &ToolCtx,
    active_tools: &ActiveTools,
    system_prompt: &str,
    agent_id: &str,
    session_id: &str,
    mut history: Vec<Message>,
    max_steps: u32,
) -> Result<String, FrameworkError> {
    let definitions = active_tools.definitions();
    let tool_names = active_tools.names();
    let run_started = Instant::now();
    let extra_instructions = dispatcher.prompt_instructions(&definitions);
    let effective_system_prompt = if extra_instructions.is_empty() {
        system_prompt.to_owned()
    } else {
        format!("{system_prompt}{extra_instructions}")
    };

    let tool_specs = if dispatcher.should_send_tool_specs() {
        definitions
    } else {
        vec![]
    };

    info!(
        status = "started",
        max_steps,
        history_len = history.len(),
        enabled_tools = ?tool_names,
        "react loop"
    );

    for step_idx in 0..max_steps {
        let step = step_idx + 1;
        let provider_started = Instant::now();
        let turn_span = info_span!("provider.turn", step, history_len = history.len());
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
            tool_call_count = response.tool_calls.len(),
            has_output_text = response.output_text.is_some(),
            "provider turn"
        );

        match dispatcher.parse_response(&response) {
            DispatchAction::ToolCalls(calls) => {
                let results = dispatcher
                    .execute_tool_calls(&calls, active_tools, tool_ctx, session_id)
                    .instrument(turn_span.clone())
                    .await;
                let messages = dispatcher.format_for_history(&calls, &results);
                history.extend(messages);
                continue;
            }
            DispatchAction::FinalResponse(text) => {
                let output_preview = sanitize_log_preview(&text, FINAL_OUTPUT_PREVIEW_CHARS);
                history.push(Message::text(Role::Assistant, text.clone()));
                info!(
                    status = "completed",
                    elapsed_ms = run_started.elapsed().as_millis() as u64,
                    output_preview = %output_preview,
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
        max_steps,
        elapsed_ms = run_started.elapsed().as_millis() as u64,
        "react loop"
    );
    Ok("max_steps reached without final response".to_owned())
}

pub(crate) fn sanitize_log_preview(text: &str, max_chars: usize) -> String {
    crate::telemetry::sanitize_preview(text, max_chars)
}

#[cfg(test)]
mod tests {
    use crate::config::ToolConfig;
    use crate::tools::default_registry;

    use super::*;

    #[test]
    fn tool_definitions_only_include_enabled_tools() {
        let config = ToolConfig {
            enabled_tools: vec![
                "memory".to_owned(),
                "summon".to_owned(),
                "clock".to_owned(),
                "read".to_owned(),
            ],
        };
        let registry = default_registry();
        let active_tools = registry
            .resolve_active(&config)
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
        let config = ToolConfig {
            enabled_tools: vec!["memory".to_owned()],
        };
        let registry = default_registry();
        let active_tools = registry
            .resolve_active(&config)
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
