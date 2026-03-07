use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, error};

use crate::config::ProviderConfig;
use crate::error::FrameworkError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub args_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default)]
    pub call_id: Option<String>,
    pub name: String,
    pub response: Value,
}

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_results: Vec::new(),
        }
    }

    pub fn tool_results(tool_results: Vec<ToolResult>) -> Self {
        Self {
            role: Role::Tool,
            content: String::new(),
            tool_calls: Vec::new(),
            tool_results,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub output_text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError>;
}

pub struct GeminiProvider {
    config: ProviderConfig,
    client: Client,
}

impl GeminiProvider {
    pub fn from_config(config: ProviderConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    fn api_key(&self) -> Result<String, FrameworkError> {
        match self.config.api_key.clone() {
            Some(api_key) if !api_key.trim().is_empty() => Ok(api_key),
            _ => Err(FrameworkError::Config(
                "missing provider API key: set provider.api_key to a ${secret:<name>} reference"
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
}

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

#[async_trait]
impl Provider for GeminiProvider {
    #[tracing::instrument(
        name = "provider.generate",
        skip(self, system_prompt, history, tools),
        fields(
            provider = "gemini",
            model = %self.config.model,
            history_len = history.len(),
            tool_count = tools.len()
        )
    )]
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError> {
        let request_started = std::time::Instant::now();
        let api_key = self.api_key()?;
        let url = self.endpoint();

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

        let body = json!({
            "system_instruction": {
                "parts": [{"text": system_prompt}]
            },
            "contents": contents,
            "tools": [{
                "functionDeclarations": function_declarations
            }]
        });
        debug!(
            status = "started",
            system_prompt_chars = system_prompt.chars().count(),
            "provider request"
        );

        let response_value = self
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
            })?
            .error_for_status()
            .map_err(|e| {
                error!(
                    status = "failed",
                    error_kind = "http_status",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %e,
                    "provider request"
                );
                FrameworkError::Provider(format!("gemini returned error: {e}"))
            })?
            .json::<Value>()
            .await
            .map_err(|e| {
                error!(
                    status = "failed",
                    error_kind = "response_parse",
                    elapsed_ms = request_started.elapsed().as_millis() as u64,
                    error = %e,
                    "provider request"
                );
                FrameworkError::Provider(format!("invalid gemini response: {e}"))
            })?;

        let mut output_text = None;
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
                    let merged = match output_text.take() {
                        Some(existing) => format!("{existing}\n{text}"),
                        None => text.to_owned(),
                    };
                    output_text = Some(merged);
                }

                if let Some(function_call) = part.get("functionCall") {
                    let name = function_call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_owned();
                    if name.is_empty() {
                        continue;
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

                    tool_calls.push(ToolCall {
                        id,
                        name,
                        args_json: args.to_string(),
                    });
                }
            }
        }

        debug!(
            status = "completed",
            elapsed_ms = request_started.elapsed().as_millis() as u64,
            output_text = output_text.is_some(),
            tool_call_count = tool_calls.len(),
            "provider request"
        );

        Ok(ProviderResponse {
            output_text,
            tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Message, Role, ToolCall, ToolResult, build_gemini_contents};

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
}
