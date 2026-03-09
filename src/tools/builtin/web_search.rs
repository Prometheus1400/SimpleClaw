use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::time::{Duration, timeout};

use crate::config::WebSearchToolConfig;
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

use super::common::parse_simple_text_arg;

const DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSearchTool {
    config: WebSearchToolConfig,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Web search using JSON: {query}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}},\"required\":[\"query\"]}"
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config).map_err(|e| {
            FrameworkError::Config(format!("tools.web_search config is invalid: {e}"))
        })?;
        Ok(())
    }

    async fn execute(
        &self,
        _ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let query = parse_simple_text_arg(args_json);
        search_duckduckgo(
            &query,
            self.config
                .timeout_seconds
                .unwrap_or(DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS),
        )
        .await
    }
}

async fn search_duckduckgo(query: &str, timeout_seconds: u64) -> Result<String, FrameworkError> {
    let client = Client::new();
    let value = timeout(Duration::from_secs(timeout_seconds), async {
        client
            .get("https://api.duckduckgo.com/")
            .query(&[
                ("q", query),
                ("format", "json"),
                ("no_redirect", "1"),
                ("no_html", "1"),
            ])
            .send()
            .await
            .map_err(|e| FrameworkError::Tool(format!("search request failed: {e}")))?
            .error_for_status()
            .map_err(|e| FrameworkError::Tool(format!("search response error: {e}")))?
            .json::<Value>()
            .await
            .map_err(|e| FrameworkError::Tool(format!("search decode failed: {e}")))
    })
    .await
    .map_err(|_| FrameworkError::Tool(format!("search timed out after {timeout_seconds}s")))??;
    Ok(summarize_duckduckgo_value(&value))
}

fn summarize_duckduckgo_value(value: &Value) -> String {
    let abstract_text = value
        .get("AbstractText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let heading = value
        .get("Heading")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if !abstract_text.is_empty() {
        let merged = format!("{heading}\n{abstract_text}");
        return merged.trim().to_owned();
    }

    let mut lines = Vec::new();
    if let Some(results) = value.get("Results").and_then(|v| v.as_array()) {
        for result in results.iter().take(5) {
            if let Some(text) = result.get("Text").and_then(|v| v.as_str()) {
                lines.push(text.to_owned());
            }
            if lines.len() >= 5 {
                break;
            }
        }
    }
    if let Some(related) = value.get("RelatedTopics").and_then(|v| v.as_array()) {
        for topic in related.iter().take(5) {
            if let Some(text) = topic.get("Text").and_then(|v| v.as_str()) {
                lines.push(text.to_owned());
            } else if let Some(topics) = topic.get("Topics").and_then(|v| v.as_array()) {
                for nested in topics.iter().take(5 - lines.len()) {
                    if let Some(text) = nested.get("Text").and_then(|v| v.as_str()) {
                        lines.push(text.to_owned());
                    }
                }
            }
            if lines.len() >= 5 {
                break;
            }
        }
    }

    if lines.is_empty() {
        "no search summary available".to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::summarize_duckduckgo_value;

    #[test]
    fn summarize_prefers_abstract_text() {
        let payload = json!({
            "Heading": "Rust",
            "AbstractText": "Rust is a systems programming language.",
            "Results": [{"Text": "ignored result"}],
            "RelatedTopics": [{"Text": "ignored topic"}]
        });

        let summary = summarize_duckduckgo_value(&payload);
        assert_eq!(summary, "Rust\nRust is a systems programming language.");
    }

    #[test]
    fn summarize_uses_results_when_abstract_missing() {
        let payload = json!({
            "Heading": "",
            "AbstractText": "",
            "Results": [
                {"Text": "Result one"},
                {"Text": "Result two"}
            ],
            "RelatedTopics": []
        });

        let summary = summarize_duckduckgo_value(&payload);
        assert_eq!(summary, "Result one\nResult two");
    }

    #[test]
    fn summarize_falls_back_to_related_topics() {
        let payload = json!({
            "Heading": "",
            "AbstractText": "",
            "Results": [],
            "RelatedTopics": [
                {"Text": "Topic one"},
                {"Topics": [{"Text": "Nested topic two"}]}
            ]
        });

        let summary = summarize_duckduckgo_value(&payload);
        assert_eq!(summary, "Topic one\nNested topic two");
    }

    #[test]
    fn summarize_returns_fallback_when_no_content_available() {
        let payload = json!({
            "Heading": "",
            "AbstractText": "",
            "Results": [],
            "RelatedTopics": []
        });

        let summary = summarize_duckduckgo_value(&payload);
        assert_eq!(summary, "no search summary available");
    }
}
