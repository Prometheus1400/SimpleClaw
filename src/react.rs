use crate::dispatch::{DispatchAction, ToolDispatcher};
use crate::error::FrameworkError;
use crate::provider::{Message, Provider, Role};
use crate::tools::{ActiveTools, ToolCtx};
use std::time::Instant;
use tracing::{debug, error, info, warn};

const FINAL_OUTPUT_PREVIEW_CHARS: usize = 120;
const REDACTED: &str = "***REDACTED***";

#[allow(clippy::too_many_arguments)]
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
        agent_id = %agent_id,
        session_id = %session_id,
        max_steps,
        history_len = history.len(),
        enabled_tools = ?tool_names,
        "agent react loop started"
    );

    for step_idx in 0..max_steps {
        let step = step_idx + 1;
        let provider_started = Instant::now();
        debug!(
            agent_id = %agent_id,
            session_id = %session_id,
            step,
            history_len = history.len(),
            "requesting provider turn"
        );
        let response = match provider
            .generate(&effective_system_prompt, &history, &tool_specs)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                error!(
                    agent_id = %agent_id,
                    session_id = %session_id,
                    step,
                    elapsed_ms = provider_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider turn failed"
                );
                return Err(err);
            }
        };
        debug!(
            agent_id = %agent_id,
            session_id = %session_id,
            step,
            elapsed_ms = provider_started.elapsed().as_millis() as u64,
            tool_call_count = response.tool_calls.len(),
            has_output_text = response.output_text.is_some(),
            "provider turn completed"
        );

        match dispatcher.parse_response(&response) {
            DispatchAction::ToolCalls(calls) => {
                let results = dispatcher
                    .execute_tool_calls(&calls, active_tools, tool_ctx, session_id)
                    .await;
                let messages = dispatcher.format_for_history(&calls, &results);
                history.extend(messages);
                continue;
            }
            DispatchAction::FinalResponse(text) => {
                let output_preview = sanitize_log_preview(&text, FINAL_OUTPUT_PREVIEW_CHARS);
                history.push(Message::text(Role::Assistant, text.clone()));
                info!(
                    agent_id = %agent_id,
                    session_id = %session_id,
                    step,
                    elapsed_ms = run_started.elapsed().as_millis() as u64,
                    output_preview = %output_preview,
                    "agent react loop completed"
                );
                return Ok(text);
            }
            DispatchAction::Empty => {
                warn!(
                    agent_id = %agent_id,
                    session_id = %session_id,
                    step,
                    "provider returned empty response"
                );
                continue;
            }
        }
    }

    warn!(
        agent_id = %agent_id,
        session_id = %session_id,
        max_steps,
        elapsed_ms = run_started.elapsed().as_millis() as u64,
        "agent react loop reached max steps without final response"
    );
    Ok("max_steps reached without final response".to_owned())
}

pub(crate) fn sanitize_log_preview(text: &str, max_chars: usize) -> String {
    truncate_for_log(
        &normalize_for_log_line(&redact_sensitive_values(text)),
        max_chars,
    )
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let clipped = text.chars().take(max_chars).collect::<String>();
    format!("{clipped}...[truncated]")
}

fn normalize_for_log_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn redact_sensitive_values(text: &str) -> String {
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) {
        redact_json_value(&mut json);
        return json.to_string();
    }

    let mut redacted = text.to_owned();
    for key in [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "access_token",
        "refresh_token",
    ] {
        redacted = redact_after_key(&redacted, key, '=');
        redacted = redact_after_key(&redacted, key, ':');
    }
    redact_bearer_token(&redacted)
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *child = serde_json::Value::String(REDACTED.to_owned());
                } else {
                    redact_json_value(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "access_token",
        "refresh_token",
    ]
    .iter()
    .any(|candidate| lower.contains(candidate))
}

fn redact_after_key(input: &str, key: &str, separator: char) -> String {
    let needle = format!("{key}{separator}");
    let mut output = input.to_owned();
    let mut search_from = 0usize;

    loop {
        if search_from >= output.len() {
            break;
        }
        let lower = output.to_ascii_lowercase();
        let Some(relative_idx) = lower[search_from..].find(&needle) else {
            break;
        };
        let token_start = search_from + relative_idx;
        let value_start = token_start + needle.len();
        if value_start >= output.len() {
            break;
        }
        let mut value_end = output[value_start..]
            .find(is_secret_value_terminator)
            .map(|idx| value_start + idx)
            .unwrap_or(output.len());

        if key.eq_ignore_ascii_case("authorization")
            && output[value_start..]
                .to_ascii_lowercase()
                .starts_with("bearer ")
        {
            let bearer_token_start = value_start + "bearer ".len();
            value_end = output[bearer_token_start..]
                .find(is_secret_value_terminator)
                .map(|idx| bearer_token_start + idx)
                .unwrap_or(output.len());
        }
        if value_end == value_start {
            search_from = value_start + 1;
            continue;
        }

        output.replace_range(value_start..value_end, REDACTED);
        search_from = value_start + REDACTED.len();
    }

    output
}

fn redact_bearer_token(input: &str) -> String {
    let mut output = input.to_owned();
    let mut search_from = 0usize;
    let needle = "bearer ";

    loop {
        if search_from >= output.len() {
            break;
        }
        let lower = output.to_ascii_lowercase();
        let Some(relative_idx) = lower[search_from..].find(needle) else {
            break;
        };
        let token_start = search_from + relative_idx;
        let value_start = token_start + needle.len();
        if value_start >= output.len() {
            break;
        }
        let value_end = output[value_start..]
            .find(is_secret_value_terminator)
            .map(|idx| value_start + idx)
            .unwrap_or(output.len());
        if value_end == value_start {
            search_from = value_start + 1;
            continue;
        }

        output.replace_range(value_start..value_end, REDACTED);
        search_from = value_start + REDACTED.len();
    }

    output
}

fn is_secret_value_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | ',' | '&' | ';' | ')' | '(' | ']' | '[' | '{' | '}'
        )
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
