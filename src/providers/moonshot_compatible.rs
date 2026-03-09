use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};
use tracing::{debug, error};

use crate::config::{MoonshotProviderConfig, ProviderAuthMode};
use crate::error::FrameworkError;

use super::types::{Message, Provider, ProviderResponse, Role, ToolCall, ToolDefinition};

pub struct MoonshotCompatibleProvider {
    model: String,
    api_base: String,
    api_key: String,
    provider_name: &'static str,
    client: Client,
}

impl MoonshotCompatibleProvider {
    pub fn from_moonshot_config(
        _provider_key: &str,
        config: MoonshotProviderConfig,
    ) -> Result<Self, FrameworkError> {
        if config.mode == ProviderAuthMode::Oauth {
            return Err(FrameworkError::Config(
                "moonshot mode oauth is not supported".to_owned(),
            ));
        }

        let api_key = config
            .api_key
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                FrameworkError::Config(
                    "missing provider API key: set providers.entries.<key>.api_key to a ${secret:<name>} reference"
                        .to_owned(),
                )
            })?;

        Ok(Self {
            model: config.model,
            api_base: config.api_base,
            api_key,
            provider_name: "moonshot",
            client: Client::new(),
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.api_base.trim_end_matches('/'))
    }

    async fn bearer_token(&self) -> Result<String, FrameworkError> {
        Ok(self.api_key.clone())
    }
}

fn build_messages(system_prompt: &str, history: &[Message]) -> Vec<Value> {
    let mut messages = Vec::new();
    if !system_prompt.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": system_prompt,
        }));
    }

    for message in history {
        if !message.tool_calls.is_empty() {
            let tool_calls: Vec<Value> = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id.clone().unwrap_or_else(|| format!("call_{}", call.name)),
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.args_json,
                        }
                    })
                })
                .collect();
            let content = if message.content.trim().is_empty() {
                Value::Null
            } else {
                Value::String(message.content.clone())
            };
            messages.push(json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            }));
            continue;
        }

        if !message.tool_results.is_empty() {
            for result in &message.tool_results {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": result.call_id.clone().unwrap_or_else(|| format!("call_{}", result.name)),
                    "name": result.name,
                    "content": result.response.to_string(),
                }));
            }
            continue;
        }

        if message.content.trim().is_empty() {
            continue;
        }

        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "user",
        };
        messages.push(json!({
            "role": role,
            "content": message.content,
        }));
    }

    messages
}

#[async_trait]
impl Provider for MoonshotCompatibleProvider {
    #[tracing::instrument(name = "provider.generate", skip(self, system_prompt, history, tools))]
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        let request_started = std::time::Instant::now();
        let bearer = self.bearer_token().await?;
        let url = self.endpoint();
        let messages = build_messages(system_prompt, history);

        let tool_specs: Vec<Value> = tools
            .iter()
            .map(|tool| {
                let schema: Value = serde_json::from_str(&tool.input_schema_json)
                    .unwrap_or_else(|_| json!({ "type": "object" }));
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": schema,
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "messages": messages,
        });
        if !tool_specs.is_empty() {
            body["tools"] = json!(tool_specs);
            body["tool_choice"] = json!("auto");
        }

        debug!(
            provider = self.provider_name,
            status = "started",
            "provider request"
        );

        let response_value = self
            .client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {bearer}"))
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                error!(
                    provider = self.provider_name,
                    status = "failed",
                    error_kind = "http_send",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider request"
                );
                FrameworkError::Provider(format!("{} request failed: {err}", self.provider_name))
            })?
            .error_for_status()
            .map_err(|err| {
                error!(
                    provider = self.provider_name,
                    status = "failed",
                    error_kind = "http_status",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider request"
                );
                FrameworkError::Provider(format!("{} returned error: {err}", self.provider_name))
            })?
            .json::<Value>()
            .await
            .map_err(|err| {
                error!(
                    provider = self.provider_name,
                    status = "failed",
                    error_kind = "response_parse",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %err,
                    "provider request"
                );
                FrameworkError::Provider(format!("invalid {} response: {err}", self.provider_name))
            })?;

        let message = response_value
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"));

        let output_text = message
            .and_then(|msg| msg.get("content"))
            .and_then(parse_content);

        let mut tool_calls = Vec::new();
        if let Some(calls) = message
            .and_then(|msg| msg.get("tool_calls"))
            .and_then(|calls| calls.as_array())
        {
            for call in calls {
                let name = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if name.is_empty() {
                    continue;
                }

                let args_json = match call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                {
                    Some(Value::String(raw)) => raw.clone(),
                    Some(other) => other.to_string(),
                    None => "{}".to_owned(),
                };

                let id = call
                    .get("id")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);

                tool_calls.push(ToolCall {
                    id,
                    name,
                    args_json,
                });
            }
        }

        debug!(
            provider = self.provider_name,
            status = "completed",
            elapsed_ms = request_started.elapsed().as_millis() as u64,
            "provider request"
        );

        Ok(ProviderResponse {
            output_text,
            tool_calls,
        })
    }
}

fn parse_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Array(parts) => {
            let mut merged = Vec::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        merged.push(trimmed.to_owned());
                    }
                }
            }
            if merged.is_empty() {
                None
            } else {
                Some(merged.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::build_messages;
    use crate::providers::{Message, Role, ToolCall, ToolResult};

    #[test]
    fn encodes_assistant_tool_calls() {
        let history = vec![Message::assistant_tool_calls(vec![ToolCall {
            id: Some("call-1".to_owned()),
            name: "clock".to_owned(),
            args_json: r#"{"timezone":"UTC"}"#.to_owned(),
        }])];

        let messages = build_messages("sys", &history);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "clock");
    }

    #[test]
    fn encodes_tool_results() {
        let history = vec![Message::tool_results(vec![ToolResult {
            call_id: Some("call-1".to_owned()),
            name: "clock".to_owned(),
            response: json!({"status":"ok","content":"2026-01-01T00:00:00Z"}),
        }])];

        let messages = build_messages("", &history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call-1");
    }

    #[test]
    fn preserves_text_messages() {
        let history = vec![Message::text(Role::User, "hello")];
        let messages = build_messages("", &history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hello");
    }
}
