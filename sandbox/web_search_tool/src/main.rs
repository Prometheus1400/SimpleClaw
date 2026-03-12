use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use tokio::time::{timeout, Duration};

const DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_WEB_SEARCH_MAX_RESULTS: usize = 5;
const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const DUCKDUCKGO_SEARCH_URL: &str = "https://api.duckduckgo.com/";

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchProvider {
    Brave,
    #[default]
    Duckduckgo,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    provider: WebSearchProvider,
    api_key: Option<String>,
    timeout_seconds: Option<u64>,
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

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed reading stdin: {e}"))?;
    let args: WebSearchArgs = serde_json::from_str(&input)
        .map_err(|e| format!("web_search requires JSON object args: {e}"))?;
    let query = args.query.trim().to_owned();
    if query.is_empty() {
        return Err("search requires a non-empty query".to_owned());
    }
    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to initialize runtime: {e}"))?;

    let output = runtime.block_on(async {
        match args.provider {
            WebSearchProvider::Brave => {
                let api_key = args
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        "search is misconfigured: api_key is required for provider=brave".to_owned()
                    })?;
                search_brave(&query, timeout_seconds, api_key).await
            }
            WebSearchProvider::Duckduckgo => search_duckduckgo(&query, timeout_seconds).await,
        }
    })?;

    let encoded =
        serde_json::to_string(&output).map_err(|e| format!("search serialization failed: {e}"))?;
    print!("{encoded}");
    Ok(())
}

async fn search_brave(
    query: &str,
    timeout_seconds: u64,
    api_key: &str,
) -> Result<WebSearchOutput, String> {
    let client = Client::new();
    let payload = timeout(Duration::from_secs(timeout_seconds), async {
        let response = client
            .get(BRAVE_SEARCH_URL)
            .query(&[("q", query), ("count", "5")])
            .header(reqwest::header::ACCEPT, "application/json")
            .header("X-Subscription-Token", api_key)
            .send()
            .await
            .map_err(|e| format!("search request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(format!(
                    "search authentication failed: provider=brave status={}",
                    status.as_u16()
                ));
            }
            return Err(format!(
                "search response error: provider=brave status={}",
                status.as_u16()
            ));
        }

        response
            .json::<BraveSearchResponse>()
            .await
            .map_err(|e| format!("search decode failed: {e}"))
    })
    .await
    .map_err(|_| format!("search timed out after {timeout_seconds}s"))??;

    Ok(map_brave_results(query, payload.web.results))
}

async fn search_duckduckgo(query: &str, timeout_seconds: u64) -> Result<WebSearchOutput, String> {
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
            .map_err(|e| format!("search request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("search response error: {e}"))?
            .json::<Value>()
            .await
            .map_err(|e| format!("search decode failed: {e}"))
    })
    .await
    .map_err(|_| format!("search timed out after {timeout_seconds}s"))??;

    Ok(map_duckduckgo_results(query, &value))
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
