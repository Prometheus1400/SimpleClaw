use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::Value;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::ToolConfig;
use crate::error::FrameworkError;
use crate::memory::MemoryStore;
use crate::provider::ToolDefinition;

#[async_trait]
pub trait SummonService: Send + Sync {
    async fn summon(
        &self,
        target_agent: &str,
        summary: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError>;
}

#[derive(Clone)]
pub struct ToolCtx {
    pub memory: MemoryStore,
    pub network_allow_all: bool,
    pub read_allow_all: bool,
    pub summon_service: Option<Arc<dyn SummonService>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Memory,
    Memorize,
    Summon,
    Search,
    Clock,
    Fetch,
    Read,
    Exec,
}

impl Tool {
    pub const fn all() -> &'static [Tool] {
        &[
            Tool::Memory,
            Tool::Memorize,
            Tool::Summon,
            Tool::Search,
            Tool::Clock,
            Tool::Fetch,
            Tool::Read,
            Tool::Exec,
        ]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Tool::Memory => "memory",
            Tool::Memorize => "memorize",
            Tool::Summon => "summon",
            Tool::Search => "search",
            Tool::Clock => "clock",
            Tool::Fetch => "fetch",
            Tool::Read => "read",
            Tool::Exec => "exec",
        }
    }

    pub fn definition(self) -> ToolDefinition {
        ToolDefinition {
            name: self.as_str().to_owned(),
            description: self.description().to_owned(),
            input_schema_json: self.input_schema_json().to_owned(),
        }
    }

    pub const fn is_enabled(self, config: &ToolConfig) -> bool {
        match self {
            Tool::Memory => config.memory,
            Tool::Memorize => config.memorize,
            Tool::Summon => config.summon,
            Tool::Search => config.search,
            Tool::Clock => config.clock,
            Tool::Fetch => config.fetch,
            Tool::Read => config.read,
            Tool::Exec => config.exec,
        }
    }

    pub async fn execute(
        self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError> {
        match self {
            Tool::Clock => Ok(Utc::now().to_rfc3339()),
            Tool::Memory => {
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
            Tool::Memorize => {
                let (fact, kind, importance) = parse_memorize_args(args_json);
                ctx.memory
                    .memorize(session_id, &fact, &kind, importance)
                    .await?;
                Ok(format!(
                    "memorized long-term fact (kind={kind}, importance={})",
                    importance.clamp(1, 5)
                ))
            }
            Tool::Summon => {
                let (target, summary) = parse_summon_args(args_json);
                let service = ctx
                    .summon_service
                    .as_ref()
                    .ok_or_else(|| FrameworkError::Tool("summon service unavailable".to_owned()))?;
                service.summon(&target, &summary, session_id).await
            }
            Tool::Search => {
                ensure_network(ctx)?;
                let query = parse_simple_text_arg(args_json);
                search_duckduckgo(&query).await
            }
            Tool::Fetch => {
                ensure_network(ctx)?;
                let url = parse_simple_text_arg(args_json);
                fetch_url_markdown(&url).await
            }
            Tool::Read => {
                if !ctx.read_allow_all {
                    return Err(FrameworkError::Tool(
                        "read tool blocked by runtime policy".to_owned(),
                    ));
                }
                let path = parse_simple_text_arg(args_json);
                let content = std::fs::read_to_string(path)?;
                Ok(content)
            }
            Tool::Exec => {
                let command = parse_exec_command_arg(args_json);
                exec_shell_command(&command).await
            }
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Tool::Memory => {
                "Semantic query short-term + long-term memory using JSON: {query, top_k?}"
            }
            Tool::Memorize => {
                "Store durable long-term memory using JSON: {fact, kind?, importance?(1-5)}"
            }
            Tool::Summon => "Synchronously hand off to another agent with JSON: {agent, summary?}",
            Tool::Search => "Web search using JSON: {query}",
            Tool::Clock => "Current timestamp",
            Tool::Fetch => "Fetch URL content using JSON: {url}",
            Tool::Read => "Read local file using JSON: {path}",
            Tool::Exec => {
                "Run a local shell command using JSON: {command}. Returns exit code, stdout, stderr."
            }
        }
    }

    const fn input_schema_json(self) -> &'static str {
        match self {
            Tool::Memory => {
                "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"},\"top_k\":{\"type\":\"integer\"}},\"required\":[\"query\"]}"
            }
            Tool::Memorize => {
                "{\"type\":\"object\",\"properties\":{\"fact\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"importance\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5}},\"required\":[\"fact\"]}"
            }
            Tool::Summon => {
                "{\"type\":\"object\",\"properties\":{\"agent\":{\"type\":\"string\"},\"summary\":{\"type\":\"string\"}},\"required\":[\"agent\"]}"
            }
            Tool::Search => {
                "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"}},\"required\":[\"query\"]}"
            }
            Tool::Clock => "{\"type\":\"null\"}",
            Tool::Fetch => {
                "{\"type\":\"object\",\"properties\":{\"url\":{\"type\":\"string\"}},\"required\":[\"url\"]}"
            }
            Tool::Read => {
                "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},\"required\":[\"path\"]}"
            }
            Tool::Exec => {
                "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"}},\"required\":[\"command\"]}"
            }
        }
    }
}

impl TryFrom<&str> for Tool {
    type Error = FrameworkError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "memory" => Ok(Tool::Memory),
            "memorize" => Ok(Tool::Memorize),
            "summon" => Ok(Tool::Summon),
            "search" => Ok(Tool::Search),
            "clock" => Ok(Tool::Clock),
            "fetch" => Ok(Tool::Fetch),
            "read" => Ok(Tool::Read),
            "exec" => Ok(Tool::Exec),
            _ => Err(FrameworkError::Tool(format!("unknown tool: {value}"))),
        }
    }
}

fn ensure_network(ctx: &ToolCtx) -> Result<(), FrameworkError> {
    if ctx.network_allow_all {
        Ok(())
    } else {
        Err(FrameworkError::Tool(
            "network tools blocked by runtime policy".to_owned(),
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

fn parse_exec_command_arg(args_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
            return command.to_owned();
        }
        if let Some(s) = value.as_str() {
            return s.to_owned();
        }
    }
    args_json.trim_matches('"').to_owned()
}

async fn exec_shell_command(command: &str) -> Result<String, FrameworkError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(FrameworkError::Tool(
            "exec requires a non-empty command".to_owned(),
        ));
    }

    let mut child = Command::new("bash");
    child.arg("-lc").arg(command);
    let output = timeout(Duration::from_secs(20), child.output())
        .await
        .map_err(|_| FrameworkError::Tool("exec timed out after 20s".to_owned()))?
        .map_err(|e| FrameworkError::Tool(format!("exec failed to start: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = truncate_for_tool_output(stdout.trim(), 8_000);
    let stderr = truncate_for_tool_output(stderr.trim(), 4_000);
    let code = output.status.code().unwrap_or(-1);

    Ok(format!(
        "exit_code: {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    ))
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
        return Ok(merged.trim().to_owned());
    }

    let mut lines = Vec::new();
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
        Ok("no search summary available".to_owned())
    } else {
        Ok(lines.join("\n"))
    }
}

async fn fetch_url_markdown(url: &str) -> Result<String, FrameworkError> {
    let body = Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| FrameworkError::Tool(format!("fetch request failed: {e}")))?
        .error_for_status()
        .map_err(|e| FrameworkError::Tool(format!("fetch response error: {e}")))?
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
