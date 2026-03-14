use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error};

use crate::config::{GeminiProviderConfig, ProviderEntryConfig};
use crate::error::FrameworkError;

use super::types::{
    Message, Provider, ProviderResponse, ProviderStream, Role, StreamEvent, ToolCall,
    ToolDefinition,
};

pub struct GeminiProvider {
    config: GeminiProviderConfig,
    client: Client,
}

impl GeminiProvider {
    pub fn from_config(config: GeminiProviderConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub fn from_entry(entry: &ProviderEntryConfig) -> Result<Self, FrameworkError> {
        let ProviderEntryConfig::Gemini(config) = entry else {
            return Err(FrameworkError::Config(
                "gemini provider received wrong provider config variant".to_owned(),
            ));
        };
        Ok(Self::from_config(config.clone()))
    }

    fn api_key(&self) -> Result<String, FrameworkError> {
        match self.config.api_key.clone() {
            Some(api_key)
                if api_key
                    .exposed()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_some() =>
            {
                Ok(api_key.exposed().expect("checked above").to_owned())
            }
            _ => Err(FrameworkError::Config(
                "missing provider API key: set providers.entries.<key>.api_key to a ${secret:<name>} reference"
                    .to_owned(),
            )),
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.config.api_base.trim_end_matches('/'),
            self.config.model
        )
    }

    fn stream_endpoint(&self) -> String {
        format!(
            "{}/models/{}:streamGenerateContent",
            self.config.api_base.trim_end_matches('/'),
            self.config.model
        )
    }
}

const ERROR_BODY_PREVIEW_CHARS: usize = 1_000;

fn build_gemini_contents(history: &[Message]) -> Vec<Value> {
    history
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::Assistant => "model",
                _ => "user",
            };

            let parts = if !message.tool_calls.is_empty() {
                message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        let args = serde_json::from_str::<Value>(&call.args_json)
                            .unwrap_or_else(|_| json!({}));
                        let mut function_call = json!({
                            "name": call.name,
                            "args": args,
                        });
                        if let Some(id) = &call.id {
                            function_call["id"] = json!(id);
                        }
                        json!({ "functionCall": function_call })
                    })
                    .collect::<Vec<_>>()
            } else if !message.tool_results.is_empty() {
                message
                    .tool_results
                    .iter()
                    .map(|result| {
                        let mut function_response = json!({
                            "name": result.name,
                            "response": result.response,
                        });
                        if let Some(call_id) = &result.call_id {
                            function_response["id"] = json!(call_id);
                        }
                        json!({ "functionResponse": function_response })
                    })
                    .collect::<Vec<_>>()
            } else if message.content.trim().is_empty() {
                Vec::new()
            } else {
                vec![json!({ "text": message.content })]
            };

            if parts.is_empty() {
                return None;
            }

            Some(json!({
                "role": role,
                "parts": parts
            }))
        })
        .collect()
}

fn preview_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn summarize_gemini_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty response body".to_owned();
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Some(error_obj) = value.get("error").and_then(Value::as_object)
    {
        let status = error_obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = error_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");

        let mut parts = Vec::new();
        if !status.is_empty() {
            parts.push(status.to_owned());
        }
        if !message.is_empty() {
            parts.push(message.to_owned());
        }

        if !parts.is_empty() {
            return parts.join(": ");
        }
    }

    preview_text(trimmed, ERROR_BODY_PREVIEW_CHARS)
}

fn build_request_body(system_prompt: &str, history: &[Message], tools: &[ToolDefinition]) -> Value {
    let function_declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let schema: Value = serde_json::from_str(&tool.input_schema_json)
                .unwrap_or_else(|_| json!({ "type": "object" }));
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": schema,
            })
        })
        .collect();

    let contents = build_gemini_contents(history);

    json!({
        "system_instruction": {
            "parts": [{"text": system_prompt}]
        },
        "contents": contents,
        "tools": [{
            "functionDeclarations": function_declarations
        }]
    })
}

fn parse_provider_response(response_value: &Value) -> ProviderResponse {
    let mut output_text: Option<String> = None;
    let mut tool_calls = Vec::new();

    if let Some(parts) = response_value
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                match output_text.as_mut() {
                    Some(existing) if !existing.is_empty() => {
                        existing.push_str(&format!("\n{text}"))
                    }
                    Some(existing) => existing.push_str(text),
                    None => output_text = Some(text.to_owned()),
                }
            }

            if let Some(function_call) = parse_tool_call_part(part) {
                tool_calls.push(function_call);
            }
        }
    }

    ProviderResponse {
        output_text,
        tool_calls,
    }
}

fn parse_tool_call_part(part: &Value) -> Option<ToolCall> {
    let function_call = part.get("functionCall")?;
    let name = function_call
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_owned();
    if name.is_empty() {
        return None;
    }
    let id = function_call
        .get("id")
        .or_else(|| function_call.get("callId"))
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let args = function_call
        .get("args")
        .cloned()
        .unwrap_or_else(|| json!({}));

    Some(ToolCall {
        id,
        name,
        args_json: args.to_string(),
    })
}

#[derive(Default)]
struct GeminiStreamAccumulator {
    line_buffer: String,
    event_data_lines: Vec<String>,
    tool_call_indices_by_id: BTreeMap<String, usize>,
    tool_calls: Vec<ToolCall>,
}

impl GeminiStreamAccumulator {
    fn push_bytes(
        &mut self,
        bytes: &[u8],
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), FrameworkError> {
        let chunk = std::str::from_utf8(bytes).map_err(|err| {
            FrameworkError::Provider(format!("invalid gemini stream bytes: {err}"))
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
        if payload.trim() == "[DONE]" {
            return Ok(());
        }

        let value: Value = serde_json::from_str(&payload).map_err(|err| {
            FrameworkError::Provider(format!("invalid gemini stream event: {err}"))
        })?;

        if let Some(parts) = value
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    events.push(StreamEvent::TextDelta(text.to_owned()));
                }

                if let Some(tool_call) = parse_tool_call_part(part) {
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
        }

        Ok(())
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    #[tracing::instrument(name = "provider.generate", skip(self, system_prompt, history, tools))]
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        let request_started = std::time::Instant::now();
        let api_key = self.api_key()?;
        let url = self.endpoint();
        let body = build_request_body(system_prompt, history, tools);
        debug!(status = "started", "provider request");

        let response = self
            .client
            .post(url)
            .query(&[("key", api_key)])
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                error!(
                    status = "failed",
                    error_kind = "http_send",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %e,
                    "provider request"
                );
                FrameworkError::Provider(format!("gemini request failed: {e}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let error_summary = summarize_gemini_error_body(&error_body);
            let error_body_preview = preview_text(&error_body, ERROR_BODY_PREVIEW_CHARS);

            error!(
                status = "failed",
                error_kind = "http_status",
                http_status = %status,
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                response_error = %error_summary,
                response_body = %error_body_preview,
                "provider request"
            );
            return Err(FrameworkError::Provider(format!(
                "gemini returned {}: {}",
                status, error_summary
            )));
        }

        let response_value = response.json::<Value>().await.map_err(|e| {
            error!(
                status = "failed",
                error_kind = "response_parse",
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                error = %e,
                "provider request"
            );
            FrameworkError::Provider(format!("invalid gemini response: {e}"))
        })?;

        debug!(
            status = "completed",
            elapsed_ms = request_started.elapsed().as_millis() as u64,
            "provider request"
        );

        Ok(parse_provider_response(&response_value))
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
        let api_key = self.api_key()?;
        let url = self.stream_endpoint();
        let body = build_request_body(system_prompt, history, tools);
        debug!(status = "started", "provider stream request");

        let response = self
            .client
            .post(url)
            .query(&[("alt", "sse"), ("key", api_key.as_str())])
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                error!(
                    status = "failed",
                    error_kind = "http_send",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %e,
                    "provider stream request"
                );
                FrameworkError::Provider(format!("gemini stream request failed: {e}"))
            })?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let error_summary = summarize_gemini_error_body(&error_body);
            let error_body_preview = preview_text(&error_body, ERROR_BODY_PREVIEW_CHARS);

            error!(
                status = "failed",
                error_kind = "http_status",
                http_status = %status,
                elapsed_ms = request_started.elapsed().as_millis() as u64,
                response_error = %error_summary,
                response_body = %error_body_preview,
                "provider stream request"
            );
            return Err(FrameworkError::Provider(format!(
                "gemini returned {}: {}",
                status, error_summary
            )));
        }

        let mut byte_stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut accumulator = GeminiStreamAccumulator::default();
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
                            "gemini stream read failed: {err}"
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
    use serde_json::json;

    use super::{
        GeminiStreamAccumulator, build_gemini_contents, parse_provider_response, preview_text,
        summarize_gemini_error_body,
    };
    use crate::providers::{Message, ProviderResponse, Role, StreamEvent, ToolCall, ToolResult};

    #[test]
    fn encodes_assistant_function_call_part() {
        let history = vec![Message::assistant_tool_calls(vec![ToolCall {
            id: Some("call-1".to_owned()),
            name: "memorize".to_owned(),
            args_json: r#"{"fact":"x"}"#.to_owned(),
        }])];
        let contents = build_gemini_contents(&history);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["name"], "memorize");
        assert_eq!(contents[0]["parts"][0]["functionCall"]["id"], "call-1");
        assert_eq!(
            contents[0]["parts"][0]["functionCall"]["args"],
            json!({"fact":"x"})
        );
    }

    #[test]
    fn encodes_tool_function_response_part() {
        let history = vec![Message::tool_results(vec![ToolResult {
            call_id: Some("call-1".to_owned()),
            name: "memorize".to_owned(),
            response: json!({"status":"ok","content":"memorized"}),
        }])];
        let contents = build_gemini_contents(&history);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(
            contents[0]["parts"][0]["functionResponse"]["name"],
            "memorize"
        );
        assert_eq!(contents[0]["parts"][0]["functionResponse"]["id"], "call-1");
        assert_eq!(
            contents[0]["parts"][0]["functionResponse"]["response"],
            json!({"status":"ok","content":"memorized"})
        );
    }

    #[test]
    fn preserves_plain_text_messages() {
        let history = vec![Message::text(Role::User, "hello")];
        let contents = build_gemini_contents(&history);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn summarizes_json_error_body() {
        let body = r#"{"error":{"code":400,"message":"Request contains an invalid argument.","status":"INVALID_ARGUMENT"}}"#;

        assert_eq!(
            summarize_gemini_error_body(body),
            "INVALID_ARGUMENT: Request contains an invalid argument."
        );
    }

    #[test]
    fn summarizes_plain_text_error_body() {
        assert_eq!(
            summarize_gemini_error_body("bad request"),
            "bad request".to_owned()
        );
    }

    #[test]
    fn preview_text_truncates_by_character_count() {
        assert_eq!(preview_text("abcdef", 4), "abcd");
    }

    #[test]
    fn parses_provider_response_text_and_tool_calls() {
        let response = parse_provider_response(&json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "hello" },
                        { "functionCall": { "name": "clock", "id": "call-1", "args": { "timezone": "UTC" } } }
                    ]
                }
            }]
        }));

        assert_eq!(
            response,
            ProviderResponse {
                output_text: Some("hello".to_owned()),
                tool_calls: vec![ToolCall {
                    id: Some("call-1".to_owned()),
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }],
            }
        );
    }

    #[test]
    fn stream_accumulator_handles_split_frames_and_buffers_tool_calls_until_finish() {
        let mut accumulator = GeminiStreamAccumulator::default();
        let mut events = Vec::new();
        accumulator
            .push_bytes(
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"},{\"functionCall\":{\"name\":\"clock\",\"id\":\"call-1\",\"args\":{\"timezone\":\"UTC\"}}}]}}]}",
                &mut events,
            )
            .expect("first chunk should parse");
        accumulator
            .push_bytes(b"\n\n", &mut events)
            .expect("second chunk should parse");
        accumulator
            .finish(&mut events)
            .expect("finish should flush remaining events");

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hel".to_owned()),
                StreamEvent::TextDelta("lo".to_owned()),
                StreamEvent::ToolCallDelta {
                    name: "clock".to_owned(),
                },
                StreamEvent::ToolCallComplete(ToolCall {
                    id: Some("call-1".to_owned()),
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn stream_accumulator_preserves_duplicate_idless_tool_calls() {
        let mut accumulator = GeminiStreamAccumulator::default();
        let mut events = Vec::new();
        accumulator
            .push_bytes(
                br#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"clock","args":{"timezone":"UTC"}}}]}}]}

data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"clock","args":{"timezone":"UTC"}}}]}}]}

"#,
                &mut events,
            )
            .expect("stream frames should parse");
        accumulator
            .finish(&mut events)
            .expect("finish should flush remaining events");

        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallDelta {
                    name: "clock".to_owned(),
                },
                StreamEvent::ToolCallDelta {
                    name: "clock".to_owned(),
                },
                StreamEvent::ToolCallComplete(ToolCall {
                    id: None,
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }),
                StreamEvent::ToolCallComplete(ToolCall {
                    id: None,
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn stream_accumulator_reports_invalid_json() {
        let mut accumulator = GeminiStreamAccumulator::default();
        let mut events = Vec::new();
        let err = accumulator.push_bytes(b"data: not-json\n\n", &mut events);

        assert!(err.is_err());
    }
}
