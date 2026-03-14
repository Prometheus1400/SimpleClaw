use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error};

use crate::auth::{AuthService, OPENAI_CODEX_PROVIDER, extract_account_id_from_jwt};
use crate::config::OpenAiCodexProviderConfig;
use crate::error::FrameworkError;

use super::types::{
    Message, Provider, ProviderResponse, ProviderStream, Role, StreamEvent, ToolCall,
    ToolDefinition,
};

const OPENAI_CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_CODEX_INSTRUCTIONS: &str =
    "You are SimpleClaw, a concise and helpful coding assistant.";
const MAX_IDENTIFIER_LEN: usize = 64;
const ERROR_BODY_PREVIEW_CHARS: usize = 1_000;
const DEFAULT_REASONING_EFFORT: &str = "high";
const DEFAULT_REASONING_SUMMARY: &str = "auto";
const DEFAULT_TEXT_VERBOSITY: &str = "medium";

#[derive(Debug, Clone)]
struct RequestContext {
    access_token: String,
    account_id: Option<String>,
    input: Vec<Value>,
    tools: Vec<Value>,
    instructions: String,
}

fn resolve_instructions(system_prompt: &str) -> String {
    if system_prompt.trim().is_empty() {
        DEFAULT_CODEX_INSTRUCTIONS.to_owned()
    } else {
        system_prompt.to_owned()
    }
}

pub struct OpenAiCodexProvider {
    model: String,
    auth: AuthService,
    client: Client,
}

impl OpenAiCodexProvider {
    pub fn from_config(config: OpenAiCodexProviderConfig) -> Result<Self, FrameworkError> {
        Ok(Self {
            model: config.model,
            auth: AuthService::new_default()?,
            client: Client::new(),
        })
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

fn normalize_tool_result_output(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(items) if items.iter().all(Value::is_object) => Value::Array(items.clone()),
        _ => Value::String(value.to_string()),
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
                    "output": normalize_tool_result_output(&result.response),
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

#[derive(Default)]
struct CodexStreamAccumulator {
    line_buffer: String,
    event_data_lines: Vec<String>,
    tool_call_indices_by_id: BTreeMap<String, usize>,
    tool_calls: Vec<ToolCall>,
    saw_text_delta: bool,
    emitted_fallback_text: bool,
}

impl CodexStreamAccumulator {
    fn push_bytes(
        &mut self,
        bytes: &[u8],
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), FrameworkError> {
        let chunk = std::str::from_utf8(bytes).map_err(|err| {
            FrameworkError::Provider(format!("invalid codex stream bytes: {err}"))
        })?;
        self.line_buffer.push_str(chunk);

        while let Some(newline_index) = self.line_buffer.find('\n') {
            let mut line = self.line_buffer[..newline_index].to_owned();
            self.line_buffer.drain(..=newline_index);
            if line.ends_with('\r') {
                line.pop();
            }
            self.process_line(&line, events)?;
        }

        Ok(())
    }

    fn finish(mut self, events: &mut Vec<StreamEvent>) -> Result<(), FrameworkError> {
        if !self.line_buffer.is_empty() {
            let mut trailing = std::mem::take(&mut self.line_buffer);
            if trailing.ends_with('\r') {
                trailing.pop();
            }
            self.process_line(&trailing, events)?;
        }
        self.flush_event(events)?;

        for tool_call in self.tool_calls {
            events.push(StreamEvent::ToolCallComplete(tool_call));
        }
        events.push(StreamEvent::Done);
        Ok(())
    }

    fn process_line(
        &mut self,
        line: &str,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), FrameworkError> {
        if line.is_empty() {
            self.flush_event(events)?;
            return Ok(());
        }

        if let Some(data) = line.strip_prefix("data:") {
            self.event_data_lines.push(data.trim_start().to_owned());
        }

        Ok(())
    }

    fn flush_event(&mut self, events: &mut Vec<StreamEvent>) -> Result<(), FrameworkError> {
        if self.event_data_lines.is_empty() {
            return Ok(());
        }

        let payload = self.event_data_lines.join("\n");
        self.event_data_lines.clear();
        let trimmed = payload.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(());
        }

        let mut event_values = Vec::new();
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            event_values.push(value);
        } else {
            for line in payload.lines() {
                let line = line.trim();
                if line.is_empty() || line == "[DONE]" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    event_values.push(value);
                }
            }
        }

        for event in event_values {
            self.process_event(&event, events)?;
        }

        Ok(())
    }

    fn process_event(
        &mut self,
        event: &Value,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), FrameworkError> {
        if let Some(message) = extract_stream_error_message(event) {
            return Err(FrameworkError::Provider(format!(
                "openai_codex stream error: {message}"
            )));
        }

        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("response.output_text.delta")
            && let Some(delta) = event.get("delta").and_then(Value::as_str)
            && !delta.is_empty()
        {
            self.saw_text_delta = true;
            events.push(StreamEvent::TextDelta(delta.to_owned()));
        }

        if event_type == Some("response.output_text.done")
            && !self.saw_text_delta
            && !self.emitted_fallback_text
            && let Some(text) = first_nonempty_str(event.get("text").and_then(Value::as_str))
        {
            self.emitted_fallback_text = true;
            events.push(StreamEvent::TextDelta(text));
        }

        if (event_type == Some("response.output_item.done")
            || event_type == Some("response.output_item.added"))
            && let Some(item) = event.get("item")
            && let Some(tool_call) = parse_tool_call(item)
        {
            self.record_tool_call(tool_call, events);
        }

        if (event_type == Some("response.completed") || event_type == Some("response.done"))
            && let Some(response_payload) = event.get("response")
        {
            let parsed = parse_provider_response(response_payload);
            if !self.saw_text_delta
                && !self.emitted_fallback_text
                && let Some(text) = parsed.output_text
            {
                self.emitted_fallback_text = true;
                events.push(StreamEvent::TextDelta(text));
            }
            for tool_call in parsed.tool_calls {
                self.record_tool_call(tool_call, events);
            }
        }

        Ok(())
    }

    fn record_tool_call(&mut self, tool_call: ToolCall, events: &mut Vec<StreamEvent>) {
        if let Some(id) = tool_call.id.clone() {
            if let Some(index) = self.tool_call_indices_by_id.get(&id).copied() {
                self.tool_calls[index] = tool_call;
            } else {
                self.tool_call_indices_by_id
                    .insert(id, self.tool_calls.len());
                events.push(StreamEvent::ToolCallDelta { name: tool_call.name.clone() });
                self.tool_calls.push(tool_call);
            }
        } else {
            events.push(StreamEvent::ToolCallDelta { name: tool_call.name.clone() });
            self.tool_calls.push(tool_call);
        }
    }
}

fn parse_sse_provider_response(body: &str) -> Result<ProviderResponse, FrameworkError> {
    let mut accumulator = CodexStreamAccumulator::default();
    let mut events = Vec::new();
    accumulator.push_bytes(body.as_bytes(), &mut events)?;
    accumulator.finish(&mut events)?;

    let mut output_text = String::new();
    let mut tool_calls = Vec::new();

    for event in events {
        match event {
            StreamEvent::TextDelta(delta) => output_text.push_str(&delta),
            StreamEvent::ToolCallDelta { .. } => {}
            StreamEvent::ToolCallComplete(tool_call) => tool_calls.push(tool_call),
            StreamEvent::Done => {}
            StreamEvent::Error(message) => return Err(FrameworkError::Provider(message)),
        }
    }

    Ok(ProviderResponse {
        output_text: (!output_text.is_empty()).then_some(output_text),
        tool_calls,
    })
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
    compact.chars().take(ERROR_BODY_PREVIEW_CHARS).collect()
}

fn build_request_body(
    model: &str,
    instructions: &str,
    input: &[Value],
    tools: &[Value],
    stream: bool,
) -> Value {
    let mut body = json!({
        "model": model,
        "input": input,
        "instructions": instructions,
        "store": false,
        "stream": stream,
        "text": {
            "verbosity": DEFAULT_TEXT_VERBOSITY,
        },
        "reasoning": {
            "effort": DEFAULT_REASONING_EFFORT,
            "summary": DEFAULT_REASONING_SUMMARY,
        },
        "include": ["reasoning.encrypted_content"],
    });

    if !tools.is_empty() {
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
    stream: bool,
) -> Result<reqwest::Response, FrameworkError> {
    let accept_header = if stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let mut request_builder = client
        .post(OPENAI_CODEX_RESPONSES_URL)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "pi")
        .header("accept", accept_header)
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

fn summarize_openai_codex_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(error_obj) = value.get("error").and_then(Value::as_object)
        && let Some(message) = error_obj.get("message").and_then(Value::as_str)
        && !message.trim().is_empty()
    {
        return message.trim().to_owned();
    }
    sanitize_body_for_error(body)
}

impl OpenAiCodexProvider {
    async fn build_request_context(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<RequestContext, FrameworkError> {
        let access_token = self
            .auth
            .get_valid_openai_access_token(None)
            .await
            .map_err(|err| {
                FrameworkError::Provider(format!(
                    "openai_codex auth token lookup failed: {err}"
                ))
            })?
            .ok_or_else(|| {
                FrameworkError::Provider(
                    "OpenAI Codex auth profile not found. Run `simpleclaw auth login --provider openai_codex`."
                        .to_owned(),
                )
            })?;
        let profile = self
            .auth
            .get_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .map_err(|err| {
                FrameworkError::Provider(format!("openai_codex auth profile lookup failed: {err}"))
            })?;
        let account_id = profile
            .and_then(|loaded| loaded.account_id)
            .or_else(|| extract_account_id_from_jwt(&access_token));

        Ok(RequestContext {
            access_token,
            account_id,
            input: build_input(history),
            tools: build_tools(tools),
            instructions: resolve_instructions(system_prompt),
        })
    }
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
        let context = self
            .build_request_context(system_prompt, history, tools)
            .await?;
        let request_body = build_request_body(
            &self.model,
            &context.instructions,
            &context.input,
            &context.tools,
            false,
        );

        debug!(
            status = "started",
            provider = "openai_codex",
            "provider request"
        );
        let response = send_request_attempt(
            &self.client,
            &context.access_token,
            context.account_id.as_deref(),
            &request_body,
            false,
        )
        .await
        .map_err(|err| {
            error!(
                status = "failed",
                provider = "openai_codex",
                error_kind = "http_send",
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                error = %err,
                "provider request"
            );
            err
        })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let error_summary = summarize_openai_codex_error(&error_body);
            error!(
                status = "failed",
                provider = "openai_codex",
                error_kind = "http_status",
                http_status = %status,
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                response_error = %error_summary,
                "provider request"
            );
            return Err(FrameworkError::Provider(format!(
                "openai_codex returned {status}: {error_summary}"
            )));
        }

        let parsed = decode_responses_body(response).await?;
        debug!(
            status = "completed",
            provider = "openai_codex",
            elapsed_ms = request_started.elapsed().as_millis() as u64,
            "provider request"
        );
        Ok(parsed)
    }

    #[tracing::instrument(
        name = "provider.generate_stream",
        skip(self, system_prompt, history, tools)
    )]
    async fn generate_stream(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderStream, FrameworkError> {
        let request_started = std::time::Instant::now();
        let context = self
            .build_request_context(system_prompt, history, tools)
            .await?;
        let request_body = build_request_body(
            &self.model,
            &context.instructions,
            &context.input,
            &context.tools,
            true,
        );

        debug!(
            status = "started",
            provider = "openai_codex",
            "provider stream request"
        );
        let response = send_request_attempt(
            &self.client,
            &context.access_token,
            context.account_id.as_deref(),
            &request_body,
            true,
        )
        .await
        .map_err(|err| {
            error!(
                status = "failed",
                provider = "openai_codex",
                error_kind = "http_send",
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                error = %err,
                "provider stream request"
            );
            err
        })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let error_summary = summarize_openai_codex_error(&error_body);
            error!(
                status = "failed",
                provider = "openai_codex",
                error_kind = "http_status",
                http_status = %status,
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                response_error = %error_summary,
                "provider stream request"
            );
            return Err(FrameworkError::Provider(format!(
                "openai_codex returned {status}: {error_summary}"
            )));
        }

        let mut byte_stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut accumulator = CodexStreamAccumulator::default();
            while let Some(chunk) = byte_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        let mut events = Vec::new();
                        match accumulator.push_bytes(&bytes, &mut events) {
                            Ok(()) => {
                                for event in events {
                                    if tx.send(event).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(StreamEvent::Error(err.to_string()));
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(StreamEvent::Error(format!(
                            "openai_codex stream read failed: {err}"
                        )));
                        return;
                    }
                }
            }

            let mut events = Vec::new();
            match accumulator.finish(&mut events) {
                Ok(()) => {
                    for event in events {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(StreamEvent::Error(err.to_string()));
                }
            }
        });

        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
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
        assert!(input[1]["output"].is_string());
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
        assert!(input[1]["output"].is_string());
    }

    #[test]
    fn normalize_tool_result_output_preserves_array_of_objects() {
        let value = json!([{"type":"text","text":"ok"}]);
        let normalized = normalize_tool_result_output(&value);
        assert!(normalized.is_array());
        assert_eq!(normalized[0]["type"], "text");
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
    fn parse_sse_provider_response_dedupes_tool_calls_by_id() {
        let payload = r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"clock","arguments":"{\"timezone\":\"UTC\"}"}}
data: {"type":"response.done","response":{"output":[{"type":"function_call","call_id":"call_1","name":"clock","arguments":"{\"timezone\":\"America/Chicago\"}"}]}}
data: [DONE]
"#;

        let parsed = parse_sse_provider_response(payload).expect("sse should parse");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            parsed.tool_calls[0].args_json,
            r#"{"timezone":"America/Chicago"}"#
        );
    }

    #[test]
    fn parse_sse_provider_response_uses_output_text_done_without_delta() {
        let payload = r#"data: {"type":"response.output_text.done","text":"Hello from done"}
data: [DONE]
"#;
        let parsed = parse_sse_provider_response(payload).expect("sse should parse");
        assert_eq!(parsed.output_text.as_deref(), Some("Hello from done"));
    }

    #[test]
    fn build_request_body_includes_tools_without_variant_downgrade() {
        let input = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}),
        ];
        let tools = vec![
            json!({"type":"function","name":"clock","parameters":{"type":"object","properties":{}}}),
        ];

        let body = build_request_body("model-a", "inst", &input, &tools, true);
        assert!(body.get("tools").is_some());
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["text"]["verbosity"], "medium");
        assert_eq!(body["reasoning"]["effort"], "high");
    }
}
