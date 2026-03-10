use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

use crate::config::{WebSearchProvider, WebSearchToolRuntimeConfig};
use crate::error::FrameworkError;
use crate::tools::{Tool, ToolExecEnv};

use super::common::parse_simple_text_arg;

const DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_WEB_SEARCH_MAX_RESULTS: usize = 5;
const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const DUCKDUCKGO_SEARCH_URL: &str = "https://api.duckduckgo.com/";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSearchTool {
    config: WebSearchToolRuntimeConfig,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Web search using configured provider and JSON: {query}"
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
        let query = parse_simple_text_arg(args_json).trim().to_owned();
        if query.is_empty() {
            return Err(FrameworkError::Tool(
                "search requires a non-empty query".to_owned(),
            ));
        }

        let timeout_seconds = self
            .config
            .timeout_seconds
            .unwrap_or(DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS);

        let output = match self.config.provider {
            WebSearchProvider::Brave => {
                let api_key = self
                    .config
                    .api_key
                    .as_ref()
                    .map(|value: &String| value.trim())
                    .filter(|value: &&str| !value.is_empty())
                    .ok_or_else(|| {
                        FrameworkError::Tool(
                            "search is misconfigured: api_key is required for provider=brave"
                                .to_owned(),
                        )
                    })?;
                search_brave(&query, timeout_seconds, api_key).await?
            }
            WebSearchProvider::Duckduckgo => search_duckduckgo(&query, timeout_seconds).await?,
        };

        serde_json::to_string(&output)
            .map_err(|e| FrameworkError::Tool(format!("search serialization failed: {e}")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WebSearchOutput {
    provider: String,
    query: String,
    total_results_returned: usize,
    results: Vec<WebSearchResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WebSearchResult {
    title: String,
    url: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    age: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveSearchResponse {
    #[serde(default)]
    web: BraveWebPayload,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BraveWebPayload {
    #[serde(default)]
    results: Vec<BraveWebResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct BraveWebResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    age: Option<String>,
    #[serde(default)]
    page_age: Option<String>,
}

async fn search_brave(
    query: &str,
    timeout_seconds: u64,
    api_key: &str,
) -> Result<WebSearchOutput, FrameworkError> {
    debug!("tool.web_search.request provider=brave");
    let client = Client::new();
    let payload = timeout(Duration::from_secs(timeout_seconds), async {
        let response = client
            .get(BRAVE_SEARCH_URL)
            .query(&[("q", query), ("count", "5")])
            .header(reqwest::header::ACCEPT, "application/json")
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .map_err(|e| FrameworkError::Tool(format!("search request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                warn!("tool.web_search.error provider=brave auth");
                return Err(FrameworkError::Tool(format!(
                    "search authentication failed: provider=brave status={}",
                    status.as_u16()
                )));
            }
            warn!(
                "tool.web_search.error provider=brave status={}",
                status.as_u16()
            );
            return Err(FrameworkError::Tool(format!(
                "search response error: provider=brave status={}",
                status.as_u16()
            )));
        }

        response
            .json::<BraveSearchResponse>()
            .await
            .map_err(|e| FrameworkError::Tool(format!("search decode failed: {e}")))
    })
    .await
    .map_err(|_| FrameworkError::Tool(format!("search timed out after {timeout_seconds}s")))??;

    let output = map_brave_results(query, payload.web.results);
    debug!(
        "tool.web_search.response provider=brave total_results_returned={}",
        output.total_results_returned
    );
    Ok(output)
}

async fn search_duckduckgo(
    query: &str,
    timeout_seconds: u64,
) -> Result<WebSearchOutput, FrameworkError> {
    debug!("tool.web_search.request provider=duckduckgo");
    let client = Client::new();
    let value = timeout(Duration::from_secs(timeout_seconds), async {
        client
            .get(DUCKDUCKGO_SEARCH_URL)
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

    let output = map_duckduckgo_results(query, &value);
    debug!(
        "tool.web_search.response provider=duckduckgo total_results_returned={}",
        output.total_results_returned
    );
    Ok(output)
}

fn map_brave_results(query: &str, results: Vec<BraveWebResult>) -> WebSearchOutput {
    let mapped = results
        .into_iter()
        .filter_map(|result| {
            let url = result.url.trim().to_owned();
            if url.is_empty() {
                return None;
            }
            let title = result.title.trim().to_owned();
            let description = result.description.trim().to_owned();
            let age = result
                .age
                .or(result.page_age)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            Some(WebSearchResult {
                title,
                url,
                description,
                age,
            })
        })
        .take(DEFAULT_WEB_SEARCH_MAX_RESULTS)
        .collect::<Vec<_>>();

    WebSearchOutput {
        provider: "brave".to_owned(),
        query: query.to_owned(),
        total_results_returned: mapped.len(),
        results: mapped,
    }
}

fn map_duckduckgo_results(query: &str, value: &Value) -> WebSearchOutput {
    let mut results = Vec::new();

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
    let abstract_url = value
        .get("AbstractURL")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if !abstract_text.is_empty() {
        let title = if heading.is_empty() {
            query.to_owned()
        } else {
            heading.to_owned()
        };
        push_result(
            &mut results,
            WebSearchResult {
                title,
                url: abstract_url.to_owned(),
                description: abstract_text.to_owned(),
                age: None,
            },
        );
    }

    if let Some(raw_results) = value.get("Results").and_then(|v| v.as_array()) {
        for result in raw_results {
            if let Some(text) = result.get("Text").and_then(|v| v.as_str()) {
                let url = result
                    .get("FirstURL")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let (title, description) = parse_duckduckgo_text(text);
                push_result(
                    &mut results,
                    WebSearchResult {
                        title,
                        url,
                        description,
                        age: None,
                    },
                );
            }
            if results.len() >= DEFAULT_WEB_SEARCH_MAX_RESULTS {
                break;
            }
        }
    }

    if results.len() < DEFAULT_WEB_SEARCH_MAX_RESULTS {
        collect_duckduckgo_related_topics(value, &mut results);
    }

    WebSearchOutput {
        provider: "duckduckgo".to_owned(),
        query: query.to_owned(),
        total_results_returned: results.len(),
        results,
    }
}

fn collect_duckduckgo_related_topics(value: &Value, out: &mut Vec<WebSearchResult>) {
    let Some(related) = value.get("RelatedTopics").and_then(|v| v.as_array()) else {
        return;
    };
    for topic in related {
        if out.len() >= DEFAULT_WEB_SEARCH_MAX_RESULTS {
            break;
        }

        if let Some(text) = topic.get("Text").and_then(|v| v.as_str()) {
            let url = topic
                .get("FirstURL")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let (title, description) = parse_duckduckgo_text(text);
            push_result(
                out,
                WebSearchResult {
                    title,
                    url,
                    description,
                    age: None,
                },
            );
            continue;
        }

        if let Some(topics) = topic.get("Topics").and_then(|v| v.as_array()) {
            for nested in topics {
                if out.len() >= DEFAULT_WEB_SEARCH_MAX_RESULTS {
                    break;
                }
                if let Some(text) = nested.get("Text").and_then(|v| v.as_str()) {
                    let url = nested
                        .get("FirstURL")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let (title, description) = parse_duckduckgo_text(text);
                    push_result(
                        out,
                        WebSearchResult {
                            title,
                            url,
                            description,
                            age: None,
                        },
                    );
                }
            }
        }
    }
}

fn parse_duckduckgo_text(text: &str) -> (String, String) {
    let trimmed = text.trim();
    if let Some((title, description)) = trimmed.split_once(" - ") {
        return (title.trim().to_owned(), description.trim().to_owned());
    }
    (trimmed.to_owned(), trimmed.to_owned())
}

fn push_result(results: &mut Vec<WebSearchResult>, candidate: WebSearchResult) {
    if results.len() >= DEFAULT_WEB_SEARCH_MAX_RESULTS {
        return;
    }

    let normalized_url = candidate.url.trim();
    let normalized_description = candidate.description.trim();
    let duplicate = results.iter().any(|existing| {
        (!normalized_url.is_empty() && existing.url == normalized_url)
            || (!normalized_description.is_empty()
                && existing.description == normalized_description)
    });
    if duplicate {
        return;
    }

    results.push(WebSearchResult {
        title: candidate.title.trim().to_owned(),
        url: normalized_url.to_owned(),
        description: normalized_description.to_owned(),
        age: candidate
            .age
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        BraveWebResult, WebSearchResult, WebSearchTool, map_brave_results, map_duckduckgo_results,
        parse_duckduckgo_text, push_result,
    };
    use crate::tools::Tool;

    #[test]
    fn map_duckduckgo_prefers_abstract_text() {
        let payload = json!({
            "Heading": "Rust",
            "AbstractText": "Rust is a systems programming language.",
            "AbstractURL": "https://www.rust-lang.org/",
            "Results": [{"Text": "ignored result"}],
            "RelatedTopics": [{"Text": "ignored topic"}]
        });

        let output = map_duckduckgo_results("rust", &payload);
        assert_eq!(output.provider, "duckduckgo");
        assert_eq!(
            output.results[0].description,
            "Rust is a systems programming language."
        );
        assert_eq!(output.results[0].title, "Rust");
        assert_eq!(output.results[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn map_duckduckgo_uses_results_when_abstract_missing() {
        let payload = json!({
            "Heading": "",
            "AbstractText": "",
            "Results": [
                {"Text": "Result one - Description one", "FirstURL": "https://example.com/1"},
                {"Text": "Result two", "FirstURL": "https://example.com/2"}
            ],
            "RelatedTopics": []
        });

        let output = map_duckduckgo_results("query", &payload);
        assert_eq!(output.total_results_returned, 2);
        assert_eq!(output.results[0].title, "Result one");
        assert_eq!(output.results[0].description, "Description one");
        assert_eq!(output.results[1].title, "Result two");
    }

    #[test]
    fn map_duckduckgo_falls_back_to_related_topics() {
        let payload = json!({
            "Heading": "",
            "AbstractText": "",
            "Results": [],
            "RelatedTopics": [
                {"Text": "Topic one - Description one", "FirstURL": "https://example.com/topic1"},
                {"Topics": [{"Text": "Nested topic two", "FirstURL": "https://example.com/topic2"}]}
            ]
        });

        let output = map_duckduckgo_results("query", &payload);
        assert_eq!(output.total_results_returned, 2);
        assert_eq!(output.results[0].title, "Topic one");
        assert_eq!(output.results[0].description, "Description one");
        assert_eq!(output.results[1].title, "Nested topic two");
    }

    #[test]
    fn map_brave_results_keeps_url_and_age() {
        let results = vec![
            BraveWebResult {
                title: "Title one".to_owned(),
                url: "https://example.com/1".to_owned(),
                description: "Description one".to_owned(),
                age: Some("2 days ago".to_owned()),
                page_age: None,
            },
            BraveWebResult {
                title: "Title two".to_owned(),
                url: "https://example.com/2".to_owned(),
                description: "Description two".to_owned(),
                age: None,
                page_age: Some("2026-01-01".to_owned()),
            },
        ];

        let output = map_brave_results("rust", results);
        assert_eq!(output.provider, "brave");
        assert_eq!(output.total_results_returned, 2);
        assert_eq!(output.results[0].age.as_deref(), Some("2 days ago"));
        assert_eq!(output.results[1].age.as_deref(), Some("2026-01-01"));
    }

    #[test]
    fn parse_duckduckgo_text_splits_title_description() {
        let (title, description) = parse_duckduckgo_text("Rust - systems language");
        assert_eq!(title, "Rust");
        assert_eq!(description, "systems language");
    }

    #[test]
    fn push_result_skips_duplicates() {
        let mut results = vec![WebSearchResult {
            title: "One".to_owned(),
            url: "https://example.com/1".to_owned(),
            description: "Description".to_owned(),
            age: None,
        }];
        push_result(
            &mut results,
            WebSearchResult {
                title: "Two".to_owned(),
                url: "https://example.com/1".to_owned(),
                description: "Another".to_owned(),
                age: None,
            },
        );
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn configure_accepts_runtime_api_key_string() {
        let mut tool = WebSearchTool::default();
        tool.configure(json!({
            "provider": "brave",
            "api_key": "resolved-brave-key"
        }))
        .expect("runtime config should deserialize");

        assert_eq!(tool.config.api_key.as_deref(), Some("resolved-brave-key"));
    }
}
