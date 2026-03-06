use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, USER_AGENT};
use scraper::{Html, Selector};

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::common::parse_simple_text_arg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFetchTool {
    HttpFetch,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch URL content using JSON: {url}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"url\":{\"type\":\"string\"}},\"required\":[\"url\"]}"
    }

    async fn execute(
        &self,
        _ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let url = parse_simple_text_arg(args_json);
        fetch_url_markdown(&url).await
    }
}

async fn fetch_url_markdown(url: &str) -> Result<String, FrameworkError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(FrameworkError::Tool(
            "fetch requires a non-empty url".to_owned(),
        ));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

    let client = Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| FrameworkError::Tool(format!("fetch client build failed: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| FrameworkError::Tool(format!("fetch request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        return Err(FrameworkError::Tool(format!(
            "fetch response error: status={} url={url}",
            status.as_u16()
        )));
    }

    let body = response
        .text()
        .await
        .map_err(|e| FrameworkError::Tool(format!("fetch body read failed: {e}")))?;

    if body.contains("<html") || body.contains("<body") {
        let doc = Html::parse_document(&body);
        let selector = Selector::parse("body")
            .map_err(|e| FrameworkError::Tool(format!("html selector parse failed: {e}")))?;
        let text = doc
            .select(&selector)
            .flat_map(|node| node.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        let clipped = text.chars().take(8_000).collect::<String>();
        return Ok(clipped);
    }

    Ok(body.chars().take(8_000).collect())
}
