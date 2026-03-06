use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, USER_AGENT};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::SandboxMode;
use crate::error::FrameworkError;
use crate::tools::{
    ProcessSnapshot, ProcessStatus, Tool, ToolCtx, ToolRegistry, wait_for_completion,
};

pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    registry.register(MemoryTool::Default);
    registry.register(MemorizeTool::Default);
    registry.register(SummonTool::Default);
    registry.register(WebSearchTool::Default);
    registry.register(ClockTool::Default);
    registry.register(WebFetchTool::Default);
    registry.register(ReadTool::Default);
    registry.register(ExecTool::Default);
    registry.register(ProcessTool::Default);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTool {
    Default,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Semantic query short-term + long-term memory using JSON: {query, top_k?}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"},\"top_k\":{\"type\":\"integer\"}},\"required\":[\"query\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (query, top_k) = parse_memory_args(args_json);
        let results = ctx
            .memory
            .semantic_query_combined(session_id, &query, top_k)
            .await?;
        if results.is_empty() {
            return Ok("no memory hits".to_owned());
        }
        Ok(results
            .iter()
            .enumerate()
            .map(|(i, hit)| format!("{}. {}", i + 1, hit))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorizeTool {
    Default,
}

#[async_trait]
impl Tool for MemorizeTool {
    fn name(&self) -> &'static str {
        "memorize"
    }

    fn description(&self) -> &'static str {
        "Store durable long-term memory using JSON: {fact, kind?, importance?(1-5)}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"fact\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"importance\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5}},\"required\":[\"fact\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (fact, kind, importance) = parse_memorize_args(args_json);
        let inserted = ctx
            .memory
            .memorize(session_id, &fact, &kind, importance)
            .await?;
        if !inserted {
            return Ok(format!(
                "already memorized long-term fact (kind={kind}, importance={})",
                importance.clamp(1, 5)
            ));
        }
        Ok(format!(
            "memorized long-term fact (kind={kind}, importance={})",
            importance.clamp(1, 5)
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonTool {
    Default,
}

#[async_trait]
impl Tool for SummonTool {
    fn name(&self) -> &'static str {
        "summon"
    }

    fn description(&self) -> &'static str {
        "Synchronously hand off to another agent with JSON: {agent, summary?}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"agent\":{\"type\":\"string\"},\"summary\":{\"type\":\"string\"}},\"required\":[\"agent\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        let (target, summary) = parse_summon_args(args_json);
        let service = ctx
            .summon_service
            .as_ref()
            .ok_or_else(|| FrameworkError::Tool("summon service unavailable".to_owned()))?;
        service.summon(&target, &summary, session_id).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchTool {
    Default,
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

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        ensure_network(ctx)?;
        let query = parse_simple_text_arg(args_json);
        search_duckduckgo(&query).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTool {
    Default,
}

#[async_trait]
impl Tool for ClockTool {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn description(&self) -> &'static str {
        "Current timestamp"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"null\"}"
    }

    async fn execute(
        &self,
        _ctx: &ToolCtx,
        _args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        Ok(Utc::now().to_rfc3339())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFetchTool {
    Default,
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
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        ensure_network(ctx)?;
        let url = parse_simple_text_arg(args_json);
        fetch_url_markdown(&url).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTool {
    Default,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read local file using JSON: {path}"
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        if !ctx.read_allow_all {
            return Err(FrameworkError::Tool(
                "read tool blocked by agent policy".to_owned(),
            ));
        }
        let path = parse_simple_text_arg(args_json);
        let content = std::fs::read_to_string(path)?;
        Ok(content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecTool {
    Default,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        "Run local shell commands using JSON: {command, background?, yield_ms?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"},\"yield_ms\":{\"type\":\"integer\",\"minimum\":0}},\"required\":[\"command\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args = parse_exec_args(args_json);
        if args.command.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "exec requires a non-empty command".to_owned(),
            ));
        }

        if args.background {
            let session_id = ctx
                .process_manager
                .spawn(
                    args.command.trim(),
                    if ctx.sandbox == SandboxMode::Workspace {
                        Some(&ctx.workspace_root)
                    } else {
                        None
                    },
                )
                .await?;
            let wait_for = Duration::from_millis(args.yield_ms.min(120_000));
            let snapshot = wait_for_completion(&ctx.process_manager, &session_id, wait_for).await?;
            if snapshot.status == ProcessStatus::Running {
                return Ok(json!({"status":"running","sessionId": session_id}).to_string());
            }
            return Ok(snapshot_to_json(&snapshot).to_string());
        }

        let result = exec_shell_command(
            args.command.trim(),
            if ctx.sandbox == SandboxMode::Workspace {
                Some(&ctx.workspace_root)
            } else {
                None
            },
        )
        .await?;
        Ok(result.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTool {
    Default,
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &'static str {
        "process"
    }

    fn description(&self) -> &'static str {
        "Manage exec background processes using JSON: {action: list|poll|kill, session_id?}. Returns JSON string."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"action\":{\"type\":\"string\",\"enum\":[\"list\",\"poll\",\"kill\"]},\"session_id\":{\"type\":\"string\"}},\"required\":[\"action\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args = parse_process_args(args_json);
        match args.action.as_str() {
            "list" => {
                let items = ctx.process_manager.list().await;
                let payload = items
                    .into_iter()
                    .map(|snapshot| snapshot_to_json(&snapshot))
                    .collect::<Vec<_>>();
                Ok(json!({"status":"ok","processes": payload}).to_string())
            }
            "poll" => {
                let session_id = args.session_id.ok_or_else(|| {
                    FrameworkError::Tool("process poll requires session_id".to_owned())
                })?;
                let snapshot = ctx.process_manager.update(&session_id).await?;
                Ok(snapshot_to_json(&snapshot).to_string())
            }
            "kill" => {
                let session_id = args.session_id.ok_or_else(|| {
                    FrameworkError::Tool("process kill requires session_id".to_owned())
                })?;
                let snapshot = ctx.process_manager.kill(&session_id).await?;
                Ok(snapshot_to_json(&snapshot).to_string())
            }
            other => Err(FrameworkError::Tool(format!(
                "process action must be one of list|poll|kill, got: {other}"
            ))),
        }
    }
}

fn ensure_network(ctx: &ToolCtx) -> Result<(), FrameworkError> {
    if ctx.network_allow_all {
        Ok(())
    } else {
        Err(FrameworkError::Tool(
            "network tools blocked by agent policy".to_owned(),
        ))
    }
}

fn parse_simple_text_arg(args_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(s) = value.as_str() {
            return s.to_owned();
        }
        if let Some(s) = value.get("query").and_then(|v| v.as_str()) {
            return s.to_owned();
        }
        if let Some(s) = value.get("url").and_then(|v| v.as_str()) {
            return s.to_owned();
        }
        if let Some(s) = value.get("path").and_then(|v| v.as_str()) {
            return s.to_owned();
        }
    }
    args_json.trim_matches('"').to_owned()
}

fn parse_memory_args(args_json: &str) -> (String, usize) {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(query) = value.get("query").and_then(|v| v.as_str()) {
            let top_k = value.get("top_k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            return (query.to_owned(), top_k.max(1));
        }
        if let Some(s) = value.as_str() {
            return (s.to_owned(), 5);
        }
    }
    (args_json.trim_matches('"').to_owned(), 5)
}

fn parse_summon_args(args_json: &str) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(agent) = value.get("agent").and_then(|v| v.as_str()) {
            let summary = value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            return (agent.to_owned(), summary);
        }
        if let Some(s) = value.as_str() {
            return (s.to_owned(), String::new());
        }
    }
    (args_json.trim_matches('"').to_owned(), String::new())
}

fn parse_memorize_args(args_json: &str) -> (String, String, u8) {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(fact) = value.get("fact").and_then(|v| v.as_str()) {
            let kind = value
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("general")
                .to_owned();
            let importance = value
                .get("importance")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .clamp(1, 5) as u8;
            return (fact.to_owned(), kind, importance);
        }
        if let Some(s) = value.as_str() {
            return (s.to_owned(), "general".to_owned(), 3);
        }
    }
    (
        args_json.trim_matches('"').to_owned(),
        "general".to_owned(),
        3,
    )
}

#[derive(Debug, Deserialize)]
struct ExecArgs {
    command: String,
    #[serde(default)]
    background: bool,
    #[serde(default = "default_exec_yield_ms")]
    yield_ms: u64,
}

fn default_exec_yield_ms() -> u64 {
    10_000
}

fn parse_exec_args(args_json: &str) -> ExecArgs {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
            let background = value
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let yield_ms = value
                .get("yield_ms")
                .or_else(|| value.get("yieldMs"))
                .and_then(|v| v.as_u64())
                .unwrap_or(default_exec_yield_ms());
            return ExecArgs {
                command: command.to_owned(),
                background,
                yield_ms,
            };
        }
        if let Some(s) = value.as_str() {
            return ExecArgs {
                command: s.to_owned(),
                background: false,
                yield_ms: default_exec_yield_ms(),
            };
        }
    }
    ExecArgs {
        command: args_json.trim_matches('"').to_owned(),
        background: false,
        yield_ms: default_exec_yield_ms(),
    }
}

#[derive(Debug, Deserialize)]
struct ProcessArgs {
    action: String,
    #[serde(default)]
    session_id: Option<String>,
}

fn parse_process_args(args_json: &str) -> ProcessArgs {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        let action = value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list")
            .to_owned();
        let session_id = value
            .get("session_id")
            .or_else(|| value.get("sessionId"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        return ProcessArgs { action, session_id };
    }
    ProcessArgs {
        action: "list".to_owned(),
        session_id: None,
    }
}

async fn exec_shell_command(
    command: &str,
    workspace: Option<&std::path::Path>,
) -> Result<Value, FrameworkError> {
    let mut child = Command::new("bash");
    child.arg("-lc").arg(command);
    if let Some(workspace) = workspace {
        child.current_dir(workspace);
    }
    let output = timeout(Duration::from_secs(20), child.output())
        .await
        .map_err(|_| FrameworkError::Tool("exec timed out after 20s".to_owned()))?
        .map_err(|e| FrameworkError::Tool(format!("exec failed to start: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(json!({
        "status": "completed",
        "exitCode": output.status.code().unwrap_or(-1),
        "stdout": truncate_for_tool_output(stdout.trim(), 8_000),
        "stderr": truncate_for_tool_output(stderr.trim(), 4_000)
    }))
}

fn snapshot_to_json(snapshot: &ProcessSnapshot) -> Value {
    let mut payload = json!({
        "status": snapshot.status.as_str(),
        "sessionId": snapshot.session_id,
        "command": snapshot.command,
        "pid": snapshot.pid,
        "startedAt": snapshot.started_at.to_rfc3339(),
        "finishedAt": snapshot.finished_at.map(|dt| dt.to_rfc3339()),
        "exitCode": snapshot.exit_code,
        "stdout": truncate_for_tool_output(snapshot.stdout.trim(), 8_000),
        "stderr": truncate_for_tool_output(snapshot.stderr.trim(), 4_000),
    });
    if snapshot.status == ProcessStatus::Running {
        payload["status"] = Value::String("running".to_owned());
    }
    payload
}

fn truncate_for_tool_output(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let clipped = text.chars().take(max_chars).collect::<String>();
    format!("{clipped}\n...[truncated]")
}

async fn search_duckduckgo(query: &str) -> Result<String, FrameworkError> {
    let client = Client::new();
    let value = client
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
        .map_err(|e| FrameworkError::Tool(format!("search decode failed: {e}")))?;
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_exec_args, parse_process_args, summarize_duckduckgo_value};

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

    #[test]
    fn parse_exec_args_accepts_yield_ms_camel_case() {
        let args = parse_exec_args(r#"{"command":"sleep 1","background":true,"yieldMs":1234}"#);
        assert_eq!(args.command, "sleep 1");
        assert!(args.background);
        assert_eq!(args.yield_ms, 1234);
    }

    #[test]
    fn parse_process_args_accepts_session_id_camel_case() {
        let args = parse_process_args(r#"{"action":"poll","sessionId":"abc"}"#);
        assert_eq!(args.action, "poll");
        assert_eq!(args.session_id.as_deref(), Some("abc"));
    }
}
