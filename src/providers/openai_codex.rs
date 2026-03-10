use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};
use tracing::{debug, error, warn};

use crate::auth::{AuthService, OPENAI_CODEX_PROVIDER, extract_account_id_from_jwt};
use crate::config::OpenAiCodexProviderConfig;
use crate::error::FrameworkError;

use super::types::{Message, Provider, ProviderResponse, Role, ToolCall, ToolDefinition};

const OPENAI_CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_CODEX_INSTRUCTIONS: &str =
    "You are SimpleClaw, a concise and helpful coding assistant.";
const MAX_IDENTIFIER_LEN: usize = 64;

pub struct OpenAiCodexProvider {
    model: String,
    auth: AuthService,
    client: Client,
}

impl OpenAiCodexProvider {
    pub fn from_config(config: OpenAiCodexProviderConfig) -> Self {
        Self {
            model: config.model,
            auth: AuthService::new_default(),
            client: Client::new(),
        }
    }
}

fn resolve_instructions(system_prompt: &str) -> String {
    if system_prompt.trim().is_empty() {
        DEFAULT_CODEX_INSTRUCTIONS.to_owned()
    } else {
        system_prompt.to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestVariant {
    Full,
    NoTools,
    Minimal,
}

impl RequestVariant {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::NoTools => "no_tools",
            Self::Minimal => "minimal",
        }
    }
}

fn sanitize_identifier(raw: &str, fallback: &str) -> String {
    let sanitized = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.chars().take(MAX_IDENTIFIER_LEN).collect()
    }
}

fn normalize_tool_arguments(args_json: &str) -> String {
    match serde_json::from_str::<Value>(args_json) {
        Ok(Value::Object(map)) => Value::Object(map).to_string(),
        Ok(_) | Err(_) => "{}".to_owned(),
    }
}

fn build_input(history: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();

    for message in history {
        if !message.tool_calls.is_empty() {
            for call in &message.tool_calls {
                let fallback_name = "tool";
                let name = sanitize_identifier(&call.name, fallback_name);
                let call_id_raw = call
                    .id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| format!("call_{name}"));
                let call_id = sanitize_identifier(&call_id_raw, &format!("call_{name}"));
                input.push(json!({
                    "type": "function_call",
                    "name": name,
                    "call_id": call_id,
                    "arguments": normalize_tool_arguments(&call.args_json),
                }));
            }
            if message.content.trim().is_empty() {
                continue;
            }
        }

        if !message.tool_results.is_empty() {
            for result in &message.tool_results {
                let fallback_call_id =
                    format!("call_{}", sanitize_identifier(&result.name, "tool"));
                let call_id_raw = result
                    .call_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(fallback_call_id.clone());
                let call_id = sanitize_identifier(&call_id_raw, &fallback_call_id);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": result.response,
                }));
            }
            if message.content.trim().is_empty() {
                continue;
            }
        }

        if message.content.trim().is_empty() {
            continue;
        }

        let (role, content_type) = match message.role {
            Role::System => ("system", "input_text"),
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
            Role::Tool => ("user", "input_text"),
        };

        input.push(json!({
            "type": "message",
            "role": role,
            "content": [
                {
                    "type": content_type,
                    "text": message.content,
                }
            ],
        }));
    }

    input
}

fn build_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = sanitize_identifier(&tool.name, "");
            if name.is_empty() {
                return None;
            }
            let schema = normalize_tool_schema(
                serde_json::from_str(&tool.input_schema_json)
                    .unwrap_or_else(|_| default_tool_schema()),
            );
            Some(json!({
                "type": "function",
                "name": name,
                "description": tool.description,
                "parameters": schema,
            }))
        })
        .collect()
}

fn default_tool_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
    })
}

fn normalize_schema_node(schema: Value) -> Value {
    let Value::Object(mut map) = schema else {
        return default_tool_schema();
    };

    if let Some(properties) = map.get_mut("properties") {
        if let Value::Object(prop_map) = properties {
            let keys = prop_map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                if let Some(entry) = prop_map.get_mut(&key) {
                    *entry = normalize_schema_node(entry.clone());
                }
            }
        } else {
            *properties = json!({});
        }
    }

    if let Some(required) = map.get_mut("required") {
        if let Value::Array(items) = required {
            items.retain(|item| item.is_string());
        } else {
            *required = Value::Array(Vec::new());
        }
    }

    if let Some(items) = map.get_mut("items") {
        *items = normalize_schema_node(items.clone());
    }

    if let Some(additional_properties) = map.get_mut("additionalProperties")
        && additional_properties.is_object()
    {
        *additional_properties = normalize_schema_node(additional_properties.clone());
    }

    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(value) = map.get_mut(key) {
            if let Value::Array(entries) = value {
                for entry in entries.iter_mut() {
                    if entry.is_object() {
                        *entry = normalize_schema_node(entry.clone());
                    }
                }
            } else {
                *value = Value::Array(Vec::new());
            }
        }
    }

    Value::Object(map)
}

fn normalize_tool_schema(schema: Value) -> Value {
    let Value::Object(mut map) = schema else {
        return default_tool_schema();
    };

    let schema_type = map.get("type").and_then(Value::as_str);
    if schema_type != Some("object") {
        map.insert("type".to_owned(), json!("object"));
    }
    if !map.contains_key("properties") || !map.get("properties").is_some_and(Value::is_object) {
        map.insert("properties".to_owned(), json!({}));
    }

    normalize_schema_node(Value::Object(map))
}

fn first_nonempty_str(value: Option<&str>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn parse_output_text(response: &Value) -> Option<String> {
    if let Some(text) = first_nonempty_str(response.get("output_text").and_then(Value::as_str)) {
        return Some(text);
    }

    let mut chunks = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(text) = first_nonempty_str(item.get("text").and_then(Value::as_str)) {
                chunks.push(text);
            }

            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let kind = part.get("type").and_then(Value::as_str);
                    if (kind == Some("output_text")
                        || kind == Some("input_text")
                        || kind == Some("text"))
                        && let Some(text) =
                            first_nonempty_str(part.get("text").and_then(Value::as_str))
                    {
                        chunks.push(text);
                    }
                }
            }
        }
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn parse_tool_call(item: &Value) -> Option<ToolCall> {
    let kind = item.get("type").and_then(Value::as_str);
    if kind != Some("function_call") && kind != Some("tool_call") {
        return None;
    }

    let name = item
        .get("name")
        .or_else(|| item.get("function").and_then(|value| value.get("name")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if name.is_empty() {
        return None;
    }

    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let args_value = item
        .get("arguments")
        .or_else(|| {
            item.get("function")
                .and_then(|value| value.get("arguments"))
        })
        .or_else(|| item.get("args"));

    let args_json = match args_value {
        Some(Value::String(raw)) => normalize_tool_arguments(raw),
        Some(Value::Object(map)) => Value::Object(map.clone()).to_string(),
        None => "{}".to_owned(),
        _ => "{}".to_owned(),
    };

    Some(ToolCall {
        id,
        name,
        args_json,
    })
}

fn collect_tool_calls(node: &Value, out: &mut Vec<ToolCall>) {
    if let Some(call) = parse_tool_call(node) {
        out.push(call);
    }

    if let Some(output) = node.get("output").and_then(Value::as_array) {
        for item in output {
            collect_tool_calls(item, out);
        }
    }
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for item in content {
            collect_tool_calls(item, out);
        }
    }
    if let Some(calls) = node.get("tool_calls").and_then(Value::as_array) {
        for item in calls {
            collect_tool_calls(item, out);
        }
    }
}

fn parse_provider_response(response: &Value) -> ProviderResponse {
    let mut tool_calls = Vec::new();
    collect_tool_calls(response, &mut tool_calls);
    let output_text = parse_output_text(response);
    ProviderResponse {
        output_text,
        tool_calls,
    }
}

fn merge_provider_responses(base: &mut ProviderResponse, incoming: ProviderResponse) {
    if base.output_text.is_none() && incoming.output_text.is_some() {
        base.output_text = incoming.output_text;
    }
    for call in incoming.tool_calls {
        if !base.tool_calls.iter().any(|existing| {
            existing.id == call.id
                && existing.name == call.name
                && existing.args_json == call.args_json
        }) {
            base.tool_calls.push(call);
        }
    }
}

fn extract_stream_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);
    if event_type == Some("error") {
        return first_nonempty_str(
            event
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| event.get("code").and_then(Value::as_str))
                .or_else(|| {
                    event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                }),
        );
    }
    if event_type == Some("response.failed") {
        return first_nonempty_str(
            event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
        );
    }
    None
}

fn parse_sse_provider_response(body: &str) -> Result<ProviderResponse, FrameworkError> {
    let mut parsed = ProviderResponse {
        output_text: None,
        tool_calls: Vec::new(),
    };
    let mut saw_delta = false;
    let mut delta_text = String::new();
    let mut saw_any_event = false;

    for chunk in body.split("\n\n") {
        let data_lines = chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "[DONE]")
            .collect::<Vec<_>>();
        if data_lines.is_empty() {
            continue;
        }
        saw_any_event = true;

        let joined = data_lines.join("\n");
        let mut event_values = Vec::new();
        if let Ok(value) = serde_json::from_str::<Value>(&joined) {
            event_values.push(value);
        } else {
            for line in data_lines {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    event_values.push(value);
                }
            }
        }

        for event in event_values {
            if let Some(message) = extract_stream_error_message(&event) {
                if parsed.output_text.is_some() || !parsed.tool_calls.is_empty() || saw_delta {
                    warn!(
                        provider = "openai_codex",
                        message = %message,
                        "ignoring stream error after partial response"
                    );
                    continue;
                }
                return Err(FrameworkError::Provider(format!(
                    "openai_codex stream error: {message}"
                )));
            }

            let event_type = event.get("type").and_then(Value::as_str);
            if event_type == Some("response.output_text.delta")
                && let Some(delta) = event.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                saw_delta = true;
                delta_text.push_str(delta);
            }

            if (event_type == Some("response.output_item.done")
                || event_type == Some("response.output_item.added"))
                && let Some(item) = event.get("item")
            {
                let wrapped = json!({ "output": [item.clone()] });
                merge_provider_responses(&mut parsed, parse_provider_response(&wrapped));
            }

            if (event_type == Some("response.completed") || event_type == Some("response.done"))
                && let Some(response_payload) = event.get("response")
            {
                merge_provider_responses(&mut parsed, parse_provider_response(response_payload));
            }
        }
    }

    if saw_delta {
        let trimmed = delta_text.trim();
        if !trimmed.is_empty() {
            parsed.output_text = Some(trimmed.to_owned());
        }
    }

    if !saw_any_event {
        return Ok(parsed);
    }

    Ok(parsed)
}

async fn decode_responses_body(
    response: reqwest::Response,
) -> Result<ProviderResponse, FrameworkError> {
    let body = response.text().await.map_err(|err| {
        FrameworkError::Provider(format!("failed to read openai_codex response body: {err}"))
    })?;

    let trimmed = body.trim_start();
    if trimmed.starts_with("data:") || trimmed.starts_with("event:") {
        return parse_sse_provider_response(&body);
    }

    let value = serde_json::from_str::<Value>(&body).map_err(|err| {
        FrameworkError::Provider(format!(
            "invalid openai_codex JSON response: {err}. Payload: {}",
            sanitize_body_for_error(&body)
        ))
    })?;
    Ok(parse_provider_response(&value))
}

fn sanitize_body_for_error(body: &str) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(800).collect()
}

fn build_request_body(
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: &[Value],
    variant: RequestVariant,
) -> Value {
    let mut body = json!({
        "model": model,
        "input": input,
        "instructions": instructions,
        "stream": true,
    });

    if variant != RequestVariant::Minimal {
        body["store"] = json!(false);
    }

    if variant == RequestVariant::Full && !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }

    body
}

async fn send_request_attempt(
    client: &Client,
    access_token: &str,
    account_id: Option<&str>,
    body: &Value,
) -> Result<reqwest::Response, FrameworkError> {
    let mut request_builder = client
        .post(OPENAI_CODEX_RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "pi")
        .header("accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .json(body);

    if let Some(account_id) = account_id {
        request_builder = request_builder.header("chatgpt-account-id", account_id);
    }

    request_builder
        .send()
        .await
        .map_err(|err| FrameworkError::Provider(format!("openai_codex request failed: {err}")))
}

#[async_trait]
impl Provider for OpenAiCodexProvider {
    #[tracing::instrument(name = "provider.generate", skip(self, system_prompt, history, tools))]
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        let request_started = std::time::Instant::now();
        let access_token = self
            .auth
            .get_valid_openai_access_token(None)
            .await?
            .ok_or_else(|| {
                FrameworkError::Provider(
                    "OpenAI Codex auth profile not found. Run `simpleclaw auth login --provider openai_codex`."
                        .to_owned(),
                )
            })?;
        let profile = self.auth.get_profile(OPENAI_CODEX_PROVIDER, None).await?;
        let account_id = profile
            .and_then(|loaded| loaded.account_id)
            .or_else(|| extract_account_id_from_jwt(&access_token));

        let input = build_input(history);
        let tools = build_tools(tools);
        let instructions = resolve_instructions(system_prompt);
        let mut attempts = vec![RequestVariant::Full];
        if !tools.is_empty() {
            attempts.push(RequestVariant::NoTools);
        }
        attempts.push(RequestVariant::Minimal);

        debug!(
            status = "started",
            provider = "openai_codex",
            "provider request"
        );
        for (index, variant) in attempts.iter().enumerate() {
            let request_body =
                build_request_body(&self.model, &instructions, &input, &tools, *variant);
            let response = send_request_attempt(
                &self.client,
                &access_token,
                account_id.as_deref(),
                &request_body,
            )
            .await
            .map_err(|err| {
                error!(
                    status = "failed",
                    provider = "openai_codex",
                    error_kind = "http_send",
                    variant = variant.as_str(),
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider request"
                );
                err
            })?;

            if response.status().is_success() {
                let parsed = decode_responses_body(response).await?;
                debug!(
                    status = "completed",
                    provider = "openai_codex",
                    variant = variant.as_str(),
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    "provider request"
                );
                return Ok(parsed);
            }

            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let sanitized = sanitize_body_for_error(&body);
            let has_more_attempts = index + 1 < attempts.len();
            if status.as_u16() == 400 && has_more_attempts {
                warn!(
                    provider = "openai_codex",
                    status = %status,
                    variant = variant.as_str(),
                    next_variant = attempts[index + 1].as_str(),
                    error = %sanitized,
                    "retrying openai_codex request with relaxed payload"
                );
                continue;
            }

            return Err(FrameworkError::Provider(format!(
                "openai_codex returned error ({status}): {sanitized}"
            )));
        }

        Err(FrameworkError::Provider(
            "openai_codex request failed after retries".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_input_encodes_native_tool_roundtrip() {
        let history = vec![
            Message::assistant_tool_calls(vec![ToolCall {
                id: Some("call-1".to_owned()),
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"UTC"}"#.to_owned(),
            }]),
            Message::tool_results(vec![crate::providers::ToolResult {
                call_id: Some("call-1".to_owned()),
                name: "clock".to_owned(),
                response: json!({"status":"ok","content":"2026-01-01T00:00:00Z"}),
            }]),
        ];

        let input = build_input(&history);
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
    }

    #[test]
    fn build_input_sanitizes_names_ids_and_arguments() {
        let history = vec![
            Message::assistant_tool_calls(vec![ToolCall {
                id: Some(" call id / 1 ".to_owned()),
                name: "clock/time".to_owned(),
                args_json: "not json".to_owned(),
            }]),
            Message::tool_results(vec![crate::providers::ToolResult {
                call_id: Some(" call id / 1 ".to_owned()),
                name: "clock/time".to_owned(),
                response: json!({"ok":true}),
            }]),
        ];

        let input = build_input(&history);
        assert_eq!(input[0]["name"], "clock_time");
        assert_eq!(input[0]["call_id"], "call_id___1");
        assert_eq!(input[0]["arguments"], "{}");
        assert_eq!(input[1]["call_id"], "call_id___1");
    }

    #[test]
    fn parse_provider_response_extracts_tool_calls() {
        let response = json!({
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "memory",
                    "arguments": "{\"query\":\"ping\"}"
                }
            ]
        });

        let parsed = parse_provider_response(&response);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "memory");
        assert_eq!(parsed.tool_calls[0].id.as_deref(), Some("call-1"));
    }

    #[test]
    fn parse_provider_response_extracts_output_text() {
        let response = json!({
            "output": [
                {
                    "type": "message",
                    "content": [
                        {"type":"output_text","text":"hello"}
                    ]
                }
            ]
        });

        let parsed = parse_provider_response(&response);
        assert_eq!(parsed.output_text.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_sse_provider_response_reads_delta_and_tool_call() {
        let payload = r#"data: {"type":"response.output_text.delta","delta":"Hello"}

data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"clock","arguments":"{\"timezone\":\"UTC\"}"}}
data: {"type":"response.done","response":{"output_text":"Hello","output":[{"type":"function_call","call_id":"call_1","name":"clock","arguments":"{\"timezone\":\"UTC\"}"}]}}
data: [DONE]
"#;

        let parsed = parse_sse_provider_response(payload).expect("sse should parse");
        assert_eq!(parsed.output_text.as_deref(), Some("Hello"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "clock");
    }

    #[test]
    fn build_tools_coerces_non_object_schema() {
        let tools = vec![ToolDefinition {
            name: "clock".to_owned(),
            description: "clock".to_owned(),
            input_schema_json: "null".to_owned(),
        }];

        let built = build_tools(&tools);
        assert_eq!(built[0]["parameters"]["type"], "object");
    }

    #[test]
    fn build_tools_drops_invalid_tool_name() {
        let tools = vec![ToolDefinition {
            name: "!!!".to_owned(),
            description: "bad".to_owned(),
            input_schema_json: "{}".to_owned(),
        }];
        let built = build_tools(&tools);
        assert!(built.is_empty());
    }

    #[test]
    fn build_tools_coerces_required_to_string_array() {
        let tools = vec![ToolDefinition {
            name: "clock".to_owned(),
            description: "clock".to_owned(),
            input_schema_json: r#"{
                "type":"object",
                "properties":{"timezone":{"type":"string"}},
                "required":["timezone", 1, null]
            }"#
            .to_owned(),
        }];

        let built = build_tools(&tools);
        assert_eq!(built[0]["parameters"]["required"], json!(["timezone"]));
    }

    #[test]
    fn parse_sse_provider_response_tolerates_empty_stream() {
        let parsed =
            parse_sse_provider_response("event: ping\n\n").expect("empty stream is tolerated");
        assert!(parsed.output_text.is_none());
        assert!(parsed.tool_calls.is_empty());
    }

    #[test]
    fn build_request_body_varies_by_request_variant() {
        let input = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}),
        ];
        let tools = vec![
            json!({"type":"function","name":"clock","parameters":{"type":"object","properties":{}}}),
        ];

        let full = build_request_body("model-a", "inst", &input, &tools, RequestVariant::Full);
        assert!(full.get("tools").is_some());
        assert_eq!(full["stream"], true);
        assert_eq!(full["parallel_tool_calls"], true);

        let no_tools =
            build_request_body("model-a", "inst", &input, &tools, RequestVariant::NoTools);
        assert!(no_tools.get("tools").is_none());
        assert!(no_tools.get("parallel_tool_calls").is_none());
        assert_eq!(no_tools["stream"], true);

        let minimal =
            build_request_body("model-a", "inst", &input, &tools, RequestVariant::Minimal);
        assert!(minimal.get("tools").is_none());
        assert!(minimal.get("store").is_none());
        assert_eq!(minimal["stream"], true);
    }
}
