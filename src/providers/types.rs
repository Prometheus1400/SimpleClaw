use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use tokio_stream::Stream;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub output_text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallComplete(ToolCall),
    Done,
    Error(String),
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

pub fn provider_response_to_stream(response: ProviderResponse) -> ProviderStream {
    let mut events = Vec::new();
    if let Some(text) = response.output_text
        && !text.is_empty()
    {
        events.push(StreamEvent::TextDelta(text));
    }
    for tool_call in response.tool_calls {
        events.push(StreamEvent::ToolCallComplete(tool_call));
    }
    events.push(StreamEvent::Done);
    Box::pin(tokio_stream::iter(events))
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn generate(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderResponse, FrameworkError>;

    async fn generate_stream(
        &self,
        system_prompt: &str,
        history: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ProviderStream, FrameworkError> {
        let response = self.generate(system_prompt, history, tools).await?;
        Ok(provider_response_to_stream(response))
    }
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;

    use super::{ProviderResponse, StreamEvent, ToolCall, provider_response_to_stream};

    #[tokio::test]
    async fn provider_response_to_stream_emits_text_tool_calls_and_done() {
        let response = ProviderResponse {
            output_text: Some("hello".to_owned()),
            tool_calls: vec![ToolCall {
                id: Some("call-1".to_owned()),
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"UTC"}"#.to_owned(),
            }],
        };

        let events: Vec<StreamEvent> = provider_response_to_stream(response).collect().await;

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hello".to_owned()),
                StreamEvent::ToolCallComplete(ToolCall {
                    id: Some("call-1".to_owned()),
                    name: "clock".to_owned(),
                    args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                }),
                StreamEvent::Done,
            ]
        );
    }
}
