use crate::dispatch::{DispatchAction, ToolDispatcher};
use crate::error::FrameworkError;
use crate::providers::{Message, Provider, Role};
use crate::tools::{ActiveTools, ToolCtx};
use std::time::Instant;
use tracing::{Instrument, debug, error, info, info_span, warn};

#[cfg(test)]
const REDACTED: &str = "***REDACTED***";

/// Bundles the dependencies and identity needed for a single ReAct loop execution.
pub struct ReactContext<'a> {
    pub provider: &'a dyn Provider,
    pub dispatcher: &'a dyn ToolDispatcher,
    pub tool_ctx: &'a ToolCtx,
    pub active_tools: &'a ActiveTools,
    pub system_prompt: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub max_steps: u32,
}

#[tracing::instrument(
    name = "react.loop",
    skip(ctx, history),
    fields(session_id = %ctx.session_id, agent_id = %ctx.agent_id)
)]
pub async fn run_loop(
    ctx: &ReactContext<'_>,
    mut history: Vec<Message>,
) -> Result<String, FrameworkError> {
    let definitions = ctx.active_tools.definitions();
    let run_started = Instant::now();
    let extra_instructions = ctx.dispatcher.prompt_instructions(&definitions);
    let effective_system_prompt = if extra_instructions.is_empty() {
        ctx.system_prompt.to_owned()
    } else {
        format!("{}{extra_instructions}", ctx.system_prompt)
    };

    let tool_specs = if ctx.dispatcher.should_send_tool_specs() {
        definitions
    } else {
        vec![]
    };

    info!(status = "started", "react loop");

    for _ in 0..ctx.max_steps {
        let provider_started = Instant::now();
        let turn_span = info_span!("provider.turn");
        debug!(parent: &turn_span, status = "started", "provider turn");
        let response = match ctx
            .provider
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

        match ctx.dispatcher.parse_response(&response) {
            DispatchAction::ToolCalls(calls) => {
                let results = ctx
                    .dispatcher
                    .execute_tool_calls(&calls, ctx.active_tools, ctx.tool_ctx, ctx.session_id)
                    .instrument(turn_span.clone())
                    .await;
                let messages = ctx.dispatcher.format_for_history(&calls, &results);
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

#[cfg_attr(not(test), allow(dead_code))]
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
