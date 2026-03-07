use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::config::SandboxMode;
use crate::error::FrameworkError;
use crate::provider::{Message, ProviderResponse, Role, ToolCall, ToolDefinition, ToolResult};
use crate::react::sanitize_log_preview;
use crate::tools::sandbox::execute_tool_with_sandbox;
use crate::tools::{ActiveTools, ToolCtx};

const TOOL_ARG_PREVIEW_CHARS: usize = 160;
const TOOL_OBSERVATION_PREVIEW_CHARS: usize = 220;
const OWNER_RESTRICTED_TOOLS: &[&str] =
    &["exec", "process", "forget", "summon", "edit", "memorize"];

pub struct ParsedToolCall {
    pub name: String,
    pub arguments: Value,
    pub tool_call_id: Option<String>,
}

pub struct ToolExecutionResult {
    pub name: String,
    pub output: String,
    pub success: bool,
    pub tool_call_id: Option<String>,
}

pub enum DispatchAction {
    ToolCalls(Vec<ParsedToolCall>),
    FinalResponse(String),
    Empty,
}

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    fn parse_response(&self, response: &ProviderResponse) -> DispatchAction;

    async fn execute_tool_calls(
        &self,
        calls: &[ParsedToolCall],
        active_tools: &ActiveTools,
        tool_ctx: &ToolCtx,
        session_id: &str,
    ) -> Vec<ToolExecutionResult> {
        let mut results = Vec::with_capacity(calls.len());
        for (call_idx, call) in calls.iter().enumerate() {
            let args_json = call.arguments.to_string();
            let args_preview = sanitize_log_preview(&args_json, TOOL_ARG_PREVIEW_CHARS);
            let tool_started = std::time::Instant::now();
            debug!(
                session_id = %session_id,
                call_index = call_idx + 1,
                tool = %call.name,
                sandbox_mode = %sandbox_mode_label(tool_ctx.sandbox),
                args = %args_preview,
                "tool call started"
            );

            let (observation, status) = match active_tools.get(call.name.as_str()) {
                Some(tool) => match enforce_tool_authorization(call.name.as_str(), tool_ctx) {
                    Ok(()) => match execute_tool_with_sandbox(
                        tool.as_ref(),
                        tool_ctx,
                        &args_json,
                        session_id,
                    )
                    .await
                    {
                        Ok(ok) => (ok, "ok"),
                        Err(err) => (format!("tool_error: {err}"), "tool_error"),
                    },
                    Err(err) => (format!("tool_error: {err}"), "tool_error"),
                },
                None => (
                    format!("tool_error: unknown tool: {}", call.name),
                    "unknown",
                ),
            };
            let observation_preview =
                sanitize_log_preview(&observation, TOOL_OBSERVATION_PREVIEW_CHARS);
            let elapsed_ms = tool_started.elapsed().as_millis() as u64;

            if status == "ok" {
                debug!(
                    session_id = %session_id,
                    call_index = call_idx + 1,
                    tool = %call.name,
                    sandbox_mode = %sandbox_mode_label(tool_ctx.sandbox),
                    elapsed_ms,
                    status,
                    observation = %observation_preview,
                    "tool call completed"
                );
            } else {
                warn!(
                    session_id = %session_id,
                    call_index = call_idx + 1,
                    tool = %call.name,
                    sandbox_mode = %sandbox_mode_label(tool_ctx.sandbox),
                    elapsed_ms,
                    status,
                    observation = %observation_preview,
                    "tool call completed with issue"
                );
            }

            results.push(ToolExecutionResult {
                name: call.name.clone(),
                output: observation,
                success: status == "ok",
                tool_call_id: call.tool_call_id.clone(),
            });
        }
        results
    }

    fn format_for_history(
        &self,
        calls: &[ParsedToolCall],
        results: &[ToolExecutionResult],
    ) -> Vec<Message>;

    fn prompt_instructions(&self, tools: &[ToolDefinition]) -> String;

    fn should_send_tool_specs(&self) -> bool;
}

fn sandbox_mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::On => "on",
        SandboxMode::Off => "off",
    }
}

fn enforce_tool_authorization(tool_name: &str, tool_ctx: &ToolCtx) -> Result<(), FrameworkError> {
    if !OWNER_RESTRICTED_TOOLS.contains(&tool_name) {
        return Ok(());
    }
    if tool_ctx.owner_ids.is_empty() {
        return Err(FrameworkError::Tool(
            "owner restriction misconfigured: runtime.owner_ids is empty".to_owned(),
        ));
    }
    if tool_ctx.is_owner() {
        Ok(())
    } else {
        Err(FrameworkError::Tool(
            "permission denied: this tool is restricted to the owner".to_owned(),
        ))
    }
}

#[cfg(test)]
fn enforce_tool_authorization_for_identity(
    tool_name: &str,
    user_id: &str,
    owner_ids: &[String],
) -> Result<(), FrameworkError> {
    if !OWNER_RESTRICTED_TOOLS.contains(&tool_name) {
        return Ok(());
    }
    if owner_ids.is_empty() {
        return Err(FrameworkError::Tool(
            "owner restriction misconfigured: runtime.owner_ids is empty".to_owned(),
        ));
    }
    if crate::tools::ToolCtx::owner_allowed(user_id, owner_ids) {
        Ok(())
    } else {
        Err(FrameworkError::Tool(
            "permission denied: this tool is restricted to the owner".to_owned(),
        ))
    }
}

// --- NativeDispatcher ---

pub struct NativeDispatcher;

impl NativeDispatcher {
    fn parsed_from_provider_calls(tool_calls: &[ToolCall]) -> Vec<ParsedToolCall> {
        tool_calls
            .iter()
            .map(|tc| ParsedToolCall {
                name: tc.name.clone(),
                arguments: serde_json::from_str(&tc.args_json)
                    .unwrap_or(Value::Object(Default::default())),
                tool_call_id: tc.id.clone(),
            })
            .collect()
    }
}

#[async_trait]
impl ToolDispatcher for NativeDispatcher {
    fn parse_response(&self, response: &ProviderResponse) -> DispatchAction {
        if !response.tool_calls.is_empty() {
            return DispatchAction::ToolCalls(Self::parsed_from_provider_calls(
                &response.tool_calls,
            ));
        }
        if let Some(text) = &response.output_text {
            return DispatchAction::FinalResponse(text.clone());
        }
        DispatchAction::Empty
    }

    fn format_for_history(
        &self,
        calls: &[ParsedToolCall],
        results: &[ToolExecutionResult],
    ) -> Vec<Message> {
        let tool_calls: Vec<ToolCall> = calls
            .iter()
            .map(|c| ToolCall {
                id: c.tool_call_id.clone(),
                name: c.name.clone(),
                args_json: c.arguments.to_string(),
            })
            .collect();

        let tool_results: Vec<ToolResult> = results
            .iter()
            .map(|r| ToolResult {
                call_id: r.tool_call_id.clone(),
                name: r.name.clone(),
                response: json!({
                    "status": if r.success { "ok" } else { "tool_error" },
                    "content": r.output,
                }),
            })
            .collect();

        vec![
            Message::assistant_tool_calls(tool_calls),
            Message::tool_results(tool_results),
        ]
    }

    fn prompt_instructions(&self, _tools: &[ToolDefinition]) -> String {
        String::new()
    }

    fn should_send_tool_specs(&self) -> bool {
        true
    }
}

// --- XmlDispatcher ---

pub struct XmlDispatcher;

impl XmlDispatcher {
    fn parse_tool_calls_from_text(text: &str) -> Vec<ParsedToolCall> {
        let re = Regex::new(
            r"(?s)<tool_call>\s*<name>(.*?)</name>\s*<arguments>(.*?)</arguments>\s*</tool_call>",
        )
        .unwrap();
        re.captures_iter(text)
            .map(|cap| {
                let name = cap[1].trim().to_owned();
                let args_raw = cap[2].trim();
                let arguments =
                    serde_json::from_str(args_raw).unwrap_or(Value::Object(Default::default()));
                ParsedToolCall {
                    name,
                    arguments,
                    tool_call_id: None,
                }
            })
            .collect()
    }

    fn format_calls_xml(calls: &[ParsedToolCall]) -> String {
        let mut xml = String::new();
        for call in calls {
            xml.push_str("<tool_call>\n<name>");
            xml.push_str(&call.name);
            xml.push_str("</name>\n<arguments>");
            xml.push_str(&call.arguments.to_string());
            xml.push_str("</arguments>\n</tool_call>\n");
        }
        xml
    }

    fn format_results_xml(results: &[ToolExecutionResult]) -> String {
        let mut xml = String::new();
        for result in results {
            xml.push_str("<tool_result>\n<name>");
            xml.push_str(&result.name);
            xml.push_str("</name>\n<status>");
            xml.push_str(if result.success { "ok" } else { "error" });
            xml.push_str("</status>\n<output>");
            xml.push_str(&result.output);
            xml.push_str("</output>\n</tool_result>\n");
        }
        xml
    }
}

#[async_trait]
impl ToolDispatcher for XmlDispatcher {
    fn parse_response(&self, response: &ProviderResponse) -> DispatchAction {
        if let Some(text) = &response.output_text {
            let calls = Self::parse_tool_calls_from_text(text);
            if !calls.is_empty() {
                return DispatchAction::ToolCalls(calls);
            }
            return DispatchAction::FinalResponse(text.clone());
        }
        DispatchAction::Empty
    }

    fn format_for_history(
        &self,
        calls: &[ParsedToolCall],
        results: &[ToolExecutionResult],
    ) -> Vec<Message> {
        vec![
            Message::text(Role::Assistant, Self::format_calls_xml(calls)),
            Message::text(Role::User, Self::format_results_xml(results)),
        ]
    }

    fn prompt_instructions(&self, tools: &[ToolDefinition]) -> String {
        let mut instructions = String::from(
            "\n\n# Tool Calling Protocol\n\n\
             You have access to tools. To call a tool, include XML tags in your response:\n\n\
             ```xml\n\
             <tool_call>\n\
             <name>tool_name</name>\n\
             <arguments>{\"key\": \"value\"}</arguments>\n\
             </tool_call>\n\
             ```\n\n\
             You may call multiple tools in one response by including multiple `<tool_call>` blocks.\n\n\
             Tool results will be provided in this format:\n\n\
             ```xml\n\
             <tool_result>\n\
             <name>tool_name</name>\n\
             <status>ok</status>\n\
             <output>result text</output>\n\
             </tool_result>\n\
             ```\n\n\
             ## Available Tools\n\n",
        );
        for tool in tools {
            instructions.push_str(&format!("### {}\n\n", tool.name));
            instructions.push_str(&format!("{}\n\n", tool.description));
            instructions.push_str(&format!(
                "**Parameters:**\n```json\n{}\n```\n\n",
                tool.input_schema_json
            ));
        }
        instructions
    }

    fn should_send_tool_specs(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_parse_response_tool_calls() {
        let response = ProviderResponse {
            output_text: Some("thinking...".to_owned()),
            tool_calls: vec![ToolCall {
                id: Some("call_1".to_owned()),
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"UTC"}"#.to_owned(),
            }],
        };
        let dispatcher = NativeDispatcher;
        let action = dispatcher.parse_response(&response);
        match action {
            DispatchAction::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "clock");
                assert_eq!(calls[0].tool_call_id, Some("call_1".to_owned()));
                assert_eq!(calls[0].arguments["timezone"], "UTC");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    #[test]
    fn native_parse_response_final_text() {
        let response = ProviderResponse {
            output_text: Some("Hello!".to_owned()),
            tool_calls: vec![],
        };
        let dispatcher = NativeDispatcher;
        match dispatcher.parse_response(&response) {
            DispatchAction::FinalResponse(text) => assert_eq!(text, "Hello!"),
            _ => panic!("expected FinalResponse"),
        }
    }

    #[test]
    fn native_parse_response_empty() {
        let response = ProviderResponse {
            output_text: None,
            tool_calls: vec![],
        };
        let dispatcher = NativeDispatcher;
        assert!(matches!(
            dispatcher.parse_response(&response),
            DispatchAction::Empty
        ));
    }

    #[test]
    fn native_format_for_history() {
        let calls = vec![ParsedToolCall {
            name: "clock".to_owned(),
            arguments: json!({"timezone": "UTC"}),
            tool_call_id: Some("c1".to_owned()),
        }];
        let results = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            output: "2026-03-06T12:00:00Z".to_owned(),
            success: true,
            tool_call_id: Some("c1".to_owned()),
        }];
        let dispatcher = NativeDispatcher;
        let messages = dispatcher.format_for_history(&calls, &results);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].name, "clock");
        assert_eq!(messages[1].role, Role::Tool);
        assert_eq!(messages[1].tool_results.len(), 1);
        assert_eq!(messages[1].tool_results[0].name, "clock");
        assert_eq!(messages[1].tool_results[0].response["status"], "ok");
    }

    #[test]
    fn xml_parse_single_tool_call() {
        let text = r#"Let me check the time.
<tool_call>
<name>clock</name>
<arguments>{"timezone":"UTC"}</arguments>
</tool_call>"#;
        let calls = XmlDispatcher::parse_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "clock");
        assert_eq!(calls[0].arguments["timezone"], "UTC");
        assert!(calls[0].tool_call_id.is_none());
    }

    #[test]
    fn xml_parse_multiple_tool_calls() {
        let text = r#"<tool_call>
<name>clock</name>
<arguments>{}</arguments>
</tool_call>
<tool_call>
<name>memory</name>
<arguments>{"query":"test"}</arguments>
</tool_call>"#;
        let calls = XmlDispatcher::parse_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "clock");
        assert_eq!(calls[1].name, "memory");
    }

    #[test]
    fn xml_parse_malformed_skipped() {
        let text = r#"<tool_call>
<name>clock</name>
</tool_call>"#;
        let calls = XmlDispatcher::parse_tool_calls_from_text(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn xml_parse_empty_arguments() {
        let text = r#"<tool_call>
<name>clock</name>
<arguments>{}</arguments>
</tool_call>"#;
        let calls = XmlDispatcher::parse_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn xml_parse_response_with_tags() {
        let response = ProviderResponse {
            output_text: Some(
                r#"<tool_call>
<name>clock</name>
<arguments>{}</arguments>
</tool_call>"#
                    .to_owned(),
            ),
            tool_calls: vec![],
        };
        let dispatcher = XmlDispatcher;
        match dispatcher.parse_response(&response) {
            DispatchAction::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "clock");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    #[test]
    fn xml_parse_response_no_tags() {
        let response = ProviderResponse {
            output_text: Some("Just a normal response.".to_owned()),
            tool_calls: vec![],
        };
        let dispatcher = XmlDispatcher;
        match dispatcher.parse_response(&response) {
            DispatchAction::FinalResponse(text) => assert_eq!(text, "Just a normal response."),
            _ => panic!("expected FinalResponse"),
        }
    }

    #[test]
    fn xml_format_for_history() {
        let calls = vec![ParsedToolCall {
            name: "clock".to_owned(),
            arguments: json!({}),
            tool_call_id: None,
        }];
        let results = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            output: "12:00".to_owned(),
            success: true,
            tool_call_id: None,
        }];
        let dispatcher = XmlDispatcher;
        let messages = dispatcher.format_for_history(&calls, &results);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert!(messages[0].content.contains("<tool_call>"));
        assert!(messages[0].content.contains("clock"));
        assert_eq!(messages[1].role, Role::User);
        assert!(messages[1].content.contains("<tool_result>"));
        assert!(messages[1].content.contains("<status>ok</status>"));
    }

    #[test]
    fn xml_prompt_instructions_contains_tool_info() {
        let tools = vec![ToolDefinition {
            name: "clock".to_owned(),
            description: "Get current time".to_owned(),
            input_schema_json: r#"{"type":"object","properties":{}}"#.to_owned(),
        }];
        let dispatcher = XmlDispatcher;
        let instructions = dispatcher.prompt_instructions(&tools);
        assert!(!instructions.is_empty());
        assert!(instructions.contains("clock"));
        assert!(instructions.contains("Get current time"));
    }

    #[test]
    fn should_send_tool_specs_native_true_xml_false() {
        assert!(NativeDispatcher.should_send_tool_specs());
        assert!(!XmlDispatcher.should_send_tool_specs());
    }

    #[test]
    fn xml_multiline_arguments() {
        let text = r#"<tool_call>
<name>exec</name>
<arguments>{
  "command": "echo hello",
  "timeout": 5000
}</arguments>
</tool_call>"#;
        let calls = XmlDispatcher::parse_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].arguments["command"], "echo hello");
        assert_eq!(calls[0].arguments["timeout"], 5000);
    }

    #[test]
    fn native_format_for_history_error_case() {
        let calls = vec![ParsedToolCall {
            name: "exec".to_owned(),
            arguments: json!({"command": "fail"}),
            tool_call_id: Some("c2".to_owned()),
        }];
        let results = vec![ToolExecutionResult {
            name: "exec".to_owned(),
            output: "tool_error: command failed".to_owned(),
            success: false,
            tool_call_id: Some("c2".to_owned()),
        }];
        let dispatcher = NativeDispatcher;
        let messages = dispatcher.format_for_history(&calls, &results);
        assert_eq!(messages[1].tool_results[0].response["status"], "tool_error");
    }

    #[test]
    fn restricted_tool_rejects_empty_owner_ids() {
        let result = enforce_tool_authorization_for_identity("exec", "u1", &[]);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|err| err.to_string().contains("owner_ids is empty"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn restricted_tool_rejects_non_owner() {
        let owner_ids = vec!["owner-1".to_owned()];
        let result = enforce_tool_authorization_for_identity("exec", "u1", &owner_ids);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|err| err.to_string().contains("restricted to the owner"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn restricted_tool_allows_owner() {
        let owner_ids = vec!["owner-1".to_owned()];
        let result = enforce_tool_authorization_for_identity("exec", "owner-1", &owner_ids);
        assert!(result.is_ok());
    }

    #[test]
    fn edit_tool_is_owner_restricted() {
        let owner_ids = vec!["owner-1".to_owned()];
        let result = enforce_tool_authorization_for_identity("edit", "u1", &owner_ids);
        assert!(result.is_err());
    }

    #[test]
    fn memorize_tool_is_owner_restricted() {
        let owner_ids = vec!["owner-1".to_owned()];
        let result = enforce_tool_authorization_for_identity("memorize", "u1", &owner_ids);
        assert!(result.is_err());
    }

    #[test]
    fn unrestricted_tool_ignores_owner_rules() {
        let result = enforce_tool_authorization_for_identity("read", "u1", &[]);
        assert!(result.is_ok());
    }
}
