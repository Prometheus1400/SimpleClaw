use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, USER_AGENT};
use scraper::{Html, Selector};
use tokio::time::{Duration, timeout};

use crate::config::WebFetchToolConfig;
use crate::error::FrameworkError;
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::common::parse_simple_text_arg;

const DEFAULT_WEB_FETCH_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_WEB_FETCH_MAX_CHARS: usize = 8_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebFetchTool {
    config: WebFetchToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchPlan {
    pub url: String,
    pub timeout_seconds: u64,
    pub max_chars: usize,
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

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config).map_err(|e| {
            FrameworkError::Config(format!("tools.web_fetch config is invalid: {e}"))
        })?;
        Ok(())
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    async fn execute(
        &self,
        _ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(args_json)?;
        self.execute_direct(plan)
            .await
            .map(ToolExecutionOutcome::Completed)
    }
}

impl WebFetchTool {
    pub fn plan(&self, args_json: &str) -> Result<WebFetchPlan, FrameworkError> {
        let url = parse_simple_text_arg(args_json);
        let timeout_seconds = self
            .config
            .timeout_seconds
            .unwrap_or(DEFAULT_WEB_FETCH_TIMEOUT_SECONDS);
        let max_chars = self
            .config
            .max_chars
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_WEB_FETCH_MAX_CHARS);
        Ok(WebFetchPlan {
            url,
            timeout_seconds,
            max_chars,
        })
    }

    pub async fn execute_direct(&self, plan: WebFetchPlan) -> Result<ToolRunOutput, FrameworkError> {
        let output = fetch_url_markdown(&plan.url, plan.timeout_seconds, plan.max_chars).await?;
        Ok(ToolRunOutput::plain(output))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv,
        plan: WebFetchPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "url": plan.url,
            "timeout_seconds": plan.timeout_seconds,
            "max_chars": plan.max_chars,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize web_fetch args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.clone(),
                persona_root: ctx.persona_root.clone(),
                artifact_name: "web_fetch_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: std::time::Duration::from_secs(plan.timeout_seconds),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(FrameworkError::Tool(format!(
                "web_fetch tool failed: exit_code={} stderr={}",
                output.exit_code,
                output.stderr.trim()
            )));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

async fn fetch_url_markdown(
    url: &str,
    timeout_seconds: u64,
    max_chars: usize,
) -> Result<String, FrameworkError> {
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

    let response = timeout(Duration::from_secs(timeout_seconds), client.get(url).send())
        .await
        .map_err(|_| FrameworkError::Tool(format!("fetch timed out after {timeout_seconds}s")))?
        .map_err(|e| FrameworkError::Tool(format!("fetch request failed: {e}")))?;

    let status = response.status();
    let body = timeout(Duration::from_secs(timeout_seconds), response.text())
        .await
        .map_err(|_| FrameworkError::Tool(format!("fetch timed out after {timeout_seconds}s")))?
        .map_err(|e| FrameworkError::Tool(format!("fetch body read failed: {e}")))?;

    render_fetch_response(url, status.as_u16(), &body, max_chars)
}

fn render_fetch_response(
    url: &str,
    status_code: u16,
    body: &str,
    max_chars: usize,
) -> Result<String, FrameworkError> {
    if !(200..300).contains(&status_code) {
        return Err(FrameworkError::Tool(format!(
            "fetch response error: status={status_code} url={url}",
        )));
    }

    if body.contains("<html") || body.contains("<body") {
        let doc = Html::parse_document(body);
        let selector = Selector::parse("body")
            .map_err(|e| FrameworkError::Tool(format!("html selector parse failed: {e}")))?;
        let text = doc
            .select(&selector)
            .flat_map(|node| node.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        return Ok(text.chars().take(max_chars).collect::<String>());
    }

    Ok(body.chars().take(max_chars).collect())
}

#[cfg(test)]
mod tests {
    use super::{fetch_url_markdown, render_fetch_response};

    #[tokio::test]
    async fn fetch_rejects_empty_url() {
        let err = fetch_url_markdown("   ", 1, 100)
            .await
            .err()
            .expect("empty url should fail");

        assert!(err.to_string().contains("fetch requires a non-empty url"));
    }

    #[test]
    fn fetch_extracts_html_body_text_and_truncates() {
        let output = render_fetch_response(
            "http://example.test",
            200,
            "<html><body><h1>Hello</h1><p>from web fetch tests</p></body></html>",
            10,
        )
        .expect("html response should render");

        assert_eq!(output, "Hello from");
    }

    #[test]
    fn fetch_returns_plain_text_body_when_not_html() {
        let output =
            render_fetch_response("http://example.test", 200, "plain body from server", 100)
                .expect("plain text response should render");

        assert_eq!(output, "plain body from server");
    }

    #[test]
    fn fetch_reports_non_success_status() {
        let err = render_fetch_response("http://example.test", 404, "not found", 100)
            .err()
            .expect("404 should fail");

        assert!(err.to_string().contains("fetch response error: status=404"));
    }
}
