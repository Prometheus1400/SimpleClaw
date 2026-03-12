use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::error::FrameworkError;
use crate::tools::{AsyncToolRunSnapshot, AsyncToolRunStatus};

pub(super) fn parse_simple_text_arg(args_json: &str) -> String {
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

#[derive(Debug)]
pub(super) enum MemoryAction {
    Query {
        query: String,
        top_k: Option<usize>,
        store: Option<String>,
    },
    List {
        kind: Option<String>,
        limit: usize,
    },
}

pub(super) fn parse_memory_args(args_json: &str) -> MemoryAction {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        let action = value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("query");
        if action == "list" {
            let kind = value
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            let limit = value.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            return MemoryAction::List { kind, limit };
        }
        if let Some(query) = value.get("query").and_then(|v| v.as_str()) {
            let top_k = value
                .get("top_k")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let store = value
                .get("store")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            return MemoryAction::Query {
                query: query.to_owned(),
                top_k,
                store,
            };
        }
        if let Some(s) = value.as_str() {
            return MemoryAction::Query {
                query: s.to_owned(),
                top_k: None,
                store: None,
            };
        }
    }
    MemoryAction::Query {
        query: args_json.trim_matches('"').to_owned(),
        top_k: None,
        store: None,
    }
}

pub(super) fn parse_summon_args(args_json: &str) -> (String, String) {
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

pub(super) fn parse_task_args(args_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(prompt) = value.get("prompt").and_then(|v| v.as_str()) {
            return prompt.to_owned();
        }
        if let Some(s) = value.as_str() {
            return s.to_owned();
        }
    }
    args_json.trim_matches('"').to_owned()
}

pub(super) fn parse_react_args(args_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(emoji) = value.get("emoji").and_then(|v| v.as_str()) {
            return emoji.to_owned();
        }
        if let Some(s) = value.as_str() {
            return s.to_owned();
        }
    }
    args_json.trim_matches('"').to_owned()
}

pub(super) fn parse_memorize_args(args_json: &str) -> (String, String, u8) {
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

#[derive(Debug)]
pub(super) struct ForgetArgs {
    pub query: String,
    pub commit: bool,
    pub similarity_threshold: f32,
    pub max_matches: usize,
    pub kind: Option<String>,
}

pub(super) fn parse_forget_args(args_json: &str) -> ForgetArgs {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(query) = value.get("query").and_then(|v| v.as_str()) {
            let commit = value
                .get("commit")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let similarity_threshold = value
                .get("similarity_threshold")
                .or_else(|| value.get("similarityThreshold"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.82) as f32;
            let max_matches = value
                .get("max_matches")
                .or_else(|| value.get("maxMatches"))
                .and_then(|v| v.as_u64())
                .unwrap_or(5) as usize;
            let kind = value
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            return ForgetArgs {
                query: query.to_owned(),
                commit,
                similarity_threshold: similarity_threshold.clamp(0.0, 1.0),
                max_matches: max_matches.clamp(1, 50),
                kind,
            };
        }
        if let Some(s) = value.as_str() {
            return ForgetArgs {
                query: s.to_owned(),
                commit: false,
                similarity_threshold: 0.82,
                max_matches: 5,
                kind: None,
            };
        }
    }
    ForgetArgs {
        query: args_json.trim_matches('"').to_owned(),
        commit: false,
        similarity_threshold: 0.82,
        max_matches: 5,
        kind: None,
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ExecArgs {
    pub command: String,
    #[serde(default)]
    pub background: bool,
}

pub(super) fn parse_exec_args(args_json: &str) -> ExecArgs {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
            let background = value
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return ExecArgs {
                command: command.to_owned(),
                background,
            };
        }
        if let Some(s) = value.as_str() {
            return ExecArgs {
                command: s.to_owned(),
                background: false,
            };
        }
    }
    ExecArgs {
        command: args_json.trim_matches('"').to_owned(),
        background: false,
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct BackgroundArgs {
    pub action: String,
    pub run_id: Option<String>,
}

pub(super) fn parse_background_args(args_json: &str) -> BackgroundArgs {
    if let Ok(value) = serde_json::from_str::<Value>(args_json) {
        let action = value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list")
            .to_owned();
        let run_id = value
            .get("run_id")
            .or_else(|| value.get("runId"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        return BackgroundArgs { action, run_id };
    }
    BackgroundArgs {
        action: "list".to_owned(),
        run_id: None,
    }
}

pub(super) async fn exec_shell_command(
    command: &str,
    workspace: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
    timeout_seconds: u64,
) -> Result<Value, FrameworkError> {
    let mut child = Command::new("bash");
    child.arg("-lc").arg(command);
    child.envs(env);
    if let Some(workspace) = workspace {
        child.current_dir(workspace);
    }
    let output = timeout(Duration::from_secs(timeout_seconds), child.output())
        .await
        .map_err(|_| FrameworkError::Tool(format!("exec timed out after {timeout_seconds}s")))?
        .map_err(|e| FrameworkError::Tool(format!("exec failed to start: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(command_output_to_json(
        output.status.code().unwrap_or(-1),
        stdout.trim(),
        stderr.trim(),
    ))
}

pub(super) fn command_output_to_json(exit_code: i32, stdout: &str, stderr: &str) -> Value {
    json!({
        "status": "completed",
        "exitCode": exit_code,
        "stdout": truncate_for_tool_output(stdout, 8_000),
        "stderr": truncate_for_tool_output(stderr, 4_000)
    })
}

pub(super) fn snapshot_to_json(snapshot: &AsyncToolRunSnapshot) -> Value {
    let details = match &snapshot.details {
        crate::tools::AsyncToolRunDetails::Process(process) => json!({
            "command": process.command,
            "pid": process.pid,
            "exitCode": process.exit_code,
            "stdout": truncate_for_tool_output(process.stdout.trim(), 8_000),
            "stderr": truncate_for_tool_output(process.stderr.trim(), 4_000),
        }),
    };
    let mut payload = json!({
        "status": snapshot.status.as_str(),
        "runId": snapshot.run_id,
        "tool": snapshot.tool_name,
        "kind": snapshot.kind.as_str(),
        "summary": snapshot.summary,
        "startedAt": snapshot.started_at.to_rfc3339(),
        "finishedAt": snapshot.finished_at.map(|dt| dt.to_rfc3339()),
        "details": details,
    });
    if snapshot.status == AsyncToolRunStatus::Running {
        payload["status"] = Value::String("running".to_owned());
    }
    payload
}

fn truncate_for_tool_output(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head_budget = max_chars / 5; // ~20%
    let tail_budget = max_chars - head_budget; // ~80%
    let head: String = text.chars().take(head_budget).collect();
    let tail: String = text.chars().skip(total - tail_budget).collect();
    let truncated = total - head_budget - tail_budget;
    format!("{head}\n...[{truncated} chars truncated]...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::{parse_background_args, parse_exec_args, parse_forget_args, parse_task_args};

    #[test]
    fn parse_exec_args_accepts_background_flag() {
        let args = parse_exec_args(r#"{"command":"sleep 1","background":true}"#);
        assert_eq!(args.command, "sleep 1");
        assert!(args.background);
    }

    #[test]
    fn parse_background_args_accepts_run_id_camel_case() {
        let args = parse_background_args(r#"{"action":"status","runId":"abc"}"#);
        assert_eq!(args.action, "status");
        assert_eq!(args.run_id.as_deref(), Some("abc"));
    }

    #[test]
    fn parse_forget_args_accepts_defaults_and_camel_case() {
        let args = parse_forget_args(
            r#"{"query":"bananas","similarityThreshold":0.9,"maxMatches":3,"commit":true,"kind":"prefs"}"#,
        );
        assert_eq!(args.query, "bananas");
        assert!(args.commit);
        assert!((args.similarity_threshold - 0.9).abs() < f32::EPSILON);
        assert_eq!(args.max_matches, 3);
        assert_eq!(args.kind.as_deref(), Some("prefs"));
    }

    #[test]
    fn parse_forget_args_clamps_threshold_and_max() {
        let args = parse_forget_args(
            r#"{"query":"bananas","similarity_threshold":2.0,"max_matches":1000}"#,
        );
        assert_eq!(args.query, "bananas");
        assert!((args.similarity_threshold - 1.0).abs() < f32::EPSILON);
        assert_eq!(args.max_matches, 50);
    }

    #[test]
    fn parse_task_args_accepts_prompt_object() {
        let prompt = parse_task_args(r#"{"prompt":"summarize this file"}"#);
        assert_eq!(prompt, "summarize this file");
    }
}
