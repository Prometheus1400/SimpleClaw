use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::error::FrameworkError;
use crate::tools::{ProcessSnapshot, ProcessStatus};

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

pub(super) fn parse_memory_args(args_json: &str) -> (String, usize) {
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
    #[serde(default = "default_exec_yield_ms")]
    pub yield_ms: u64,
}

fn default_exec_yield_ms() -> u64 {
    10_000
}

pub(super) fn parse_exec_args(args_json: &str) -> ExecArgs {
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
pub(super) struct ProcessArgs {
    pub action: String,
    pub session_id: Option<String>,
}

pub(super) fn parse_process_args(args_json: &str) -> ProcessArgs {
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

pub(super) async fn exec_shell_command(
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

pub(super) fn snapshot_to_json(snapshot: &ProcessSnapshot) -> Value {
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

#[cfg(test)]
mod tests {
    use super::{parse_exec_args, parse_forget_args, parse_process_args, parse_task_args};

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
