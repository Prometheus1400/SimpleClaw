use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::error::FrameworkError;
use crate::tools::{Tool, ToolCtx};

use super::read::resolve_path_for_read;

const PREVIEW_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTool {
    LocalFileEditor,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    command: String,
    path: String,
    content: Option<String>,
    old_text: Option<String>,
    new_text: Option<String>,
    line: Option<usize>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    dry_run: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit local files using JSON: {command,path,...}. Supports create/replace/insert/delete/append with optional dry_run."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\",\"enum\":[\"create\",\"replace\",\"insert\",\"delete\",\"append\"]},\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"},\"old_text\":{\"type\":\"string\"},\"new_text\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\",\"minimum\":1},\"replace_all\":{\"type\":\"boolean\"},\"overwrite\":{\"type\":\"boolean\"},\"dry_run\":{\"type\":\"boolean\"}},\"required\":[\"command\",\"path\"]}"
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let args: EditArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("edit requires JSON object args: {e}")))?;

        let command = args.command.trim();
        if command.is_empty() {
            return Err(FrameworkError::Tool(
                "edit requires a non-empty command".to_owned(),
            ));
        }
        if args.path.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "edit requires a non-empty path".to_owned(),
            ));
        }

        let path = resolve_path_for_read(&args.path, &ctx.workspace_root, ctx.sandbox)?;
        let path_display = path.display().to_string();

        let result = match command {
            "create" => apply_create(&path, &path_display, &args)?,
            "replace" => apply_replace(&path, &path_display, &args)?,
            "insert" => apply_insert(&path, &path_display, &args)?,
            "delete" => apply_delete(&path, &path_display, &args)?,
            "append" => apply_append(&path, &path_display, &args)?,
            other => {
                return Err(FrameworkError::Tool(format!(
                    "edit command must be one of create|replace|insert|delete|append (got: {other})"
                )));
            }
        };

        Ok(result)
    }
}

fn apply_create(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit create requires content".to_owned()))?;
    let exists = path.exists();
    if exists && !args.overwrite {
        return Err(FrameworkError::Tool(format!(
            "edit create refused because file exists: {path_display} (set overwrite=true to replace)"
        )));
    }
    if !args.dry_run {
        std::fs::write(path, content)?;
    }

    Ok(json_result(
        args.dry_run,
        "create",
        path_display,
        content.len(),
        if exists { Some("") } else { None },
        content,
    ))
}

fn apply_replace(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    let old_text = args
        .old_text
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit replace requires old_text".to_owned()))?;
    let new_text = args
        .new_text
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit replace requires new_text".to_owned()))?;
    if old_text.is_empty() {
        return Err(FrameworkError::Tool(
            "edit replace requires non-empty old_text".to_owned(),
        ));
    }

    let original = std::fs::read_to_string(path)?;
    let occurrences = original.match_indices(old_text).count();
    if occurrences == 0 {
        return Err(FrameworkError::Tool(format!(
            "edit replace found no matches for old_text in {path_display}"
        )));
    }
    if occurrences > 1 && !args.replace_all {
        return Err(FrameworkError::Tool(format!(
            "edit replace found {occurrences} matches in {path_display}; set replace_all=true to replace all"
        )));
    }

    let updated = if args.replace_all {
        original.replace(old_text, new_text)
    } else {
        original.replacen(old_text, new_text, 1)
    };
    if !args.dry_run {
        std::fs::write(path, &updated)?;
    }

    Ok(json_result(
        args.dry_run,
        "replace",
        path_display,
        updated.len(),
        Some(&original),
        &updated,
    ))
}

fn apply_insert(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit insert requires content".to_owned()))?;
    let line = args
        .line
        .ok_or_else(|| FrameworkError::Tool("edit insert requires line".to_owned()))?;
    if line == 0 {
        return Err(FrameworkError::Tool(
            "edit insert line must be >= 1".to_owned(),
        ));
    }

    let original = std::fs::read_to_string(path)?;
    let idx = byte_index_for_line(&original, line).ok_or_else(|| {
        FrameworkError::Tool(format!(
            "edit insert line {line} out of range for {path_display}"
        ))
    })?;
    let mut updated = String::with_capacity(original.len() + content.len());
    updated.push_str(&original[..idx]);
    updated.push_str(content);
    updated.push_str(&original[idx..]);

    if !args.dry_run {
        std::fs::write(path, &updated)?;
    }

    Ok(json_result(
        args.dry_run,
        "insert",
        path_display,
        updated.len(),
        Some(&original),
        &updated,
    ))
}

fn apply_delete(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    let old_text = args
        .old_text
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit delete requires old_text".to_owned()))?;
    if old_text.is_empty() {
        return Err(FrameworkError::Tool(
            "edit delete requires non-empty old_text".to_owned(),
        ));
    }

    let original = std::fs::read_to_string(path)?;
    let occurrences = original.match_indices(old_text).count();
    if occurrences == 0 {
        return Err(FrameworkError::Tool(format!(
            "edit delete found no matches for old_text in {path_display}"
        )));
    }
    if occurrences > 1 && !args.replace_all {
        return Err(FrameworkError::Tool(format!(
            "edit delete found {occurrences} matches in {path_display}; set replace_all=true to delete all"
        )));
    }

    let updated = if args.replace_all {
        original.replace(old_text, "")
    } else {
        original.replacen(old_text, "", 1)
    };
    if !args.dry_run {
        std::fs::write(path, &updated)?;
    }

    Ok(json_result(
        args.dry_run,
        "delete",
        path_display,
        updated.len(),
        Some(&original),
        &updated,
    ))
}

fn apply_append(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| FrameworkError::Tool("edit append requires content".to_owned()))?;

    let original = std::fs::read_to_string(path)?;
    let mut updated = String::with_capacity(original.len() + content.len());
    updated.push_str(&original);
    updated.push_str(content);
    if !args.dry_run {
        std::fs::write(path, &updated)?;
    }

    Ok(json_result(
        args.dry_run,
        "append",
        path_display,
        updated.len(),
        Some(&original),
        &updated,
    ))
}

fn json_result(
    dry_run: bool,
    command: &str,
    path: &str,
    bytes_written: usize,
    before: Option<&str>,
    after: &str,
) -> String {
    let status = if dry_run { "dry_run" } else { "ok" };
    let diff_preview = before.map(|prev| {
        json!({
            "before": truncate_for_preview(prev),
            "after": truncate_for_preview(after),
        })
    });

    json!({
        "status": status,
        "command": command,
        "path": path,
        "bytes_written": bytes_written,
        "diff_preview": diff_preview
    })
    .to_string()
}

fn truncate_for_preview(text: &str) -> String {
    if text.chars().count() <= PREVIEW_CHARS {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(PREVIEW_CHARS).collect();
    format!("{prefix}...<truncated>")
}

fn byte_index_for_line(content: &str, line: usize) -> Option<usize> {
    if line == 1 {
        return Some(0);
    }
    let mut current_line = 1usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            current_line += 1;
            if current_line == line {
                return Some(idx + 1);
            }
        }
    }
    if line == current_line + 1 {
        Some(content.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};

    use super::{EditArgs, apply_create, apply_replace, resolve_path_for_read};
    use crate::config::SandboxMode;

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_edit_{prefix}_{nanos}"))
    }

    fn args(value: Value) -> EditArgs {
        serde_json::from_value(value).expect("args should parse")
    }

    #[test]
    fn create_and_replace_roundtrip() {
        let workspace = unique_test_dir("create_replace");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");

        apply_create(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "create",
                "path": "notes.txt",
                "content": "hello world\n"
            })),
        )
        .expect("create should work");
        apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "notes.txt",
                "old_text": "world",
                "new_text": "team"
            })),
        )
        .expect("replace should work");

        let content = fs::read_to_string(path).expect("should read notes");
        assert_eq!(content, "hello team\n");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn replace_requires_replace_all_when_ambiguous() {
        let workspace = unique_test_dir("replace_ambiguous");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");
        fs::write(&path, "a b a b\n").expect("should write notes");

        let err = apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "notes.txt",
                "old_text": "a",
                "new_text": "z"
            })),
        )
        .expect_err("replace should fail without replace_all");
        assert!(err.to_string().contains("set replace_all=true"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn dry_run_does_not_mutate_file() {
        let workspace = unique_test_dir("dry_run");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("doc.txt");
        fs::write(&path, "alpha beta\n").expect("should write doc");

        let out = apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "doc.txt",
                "old_text": "beta",
                "new_text": "gamma",
                "dry_run": true
            })),
        )
        .expect("dry run should succeed");
        let parsed: Value = serde_json::from_str(&out).expect("output should be json");
        assert_eq!(parsed["status"], "dry_run");
        let content = fs::read_to_string(path).expect("should read doc");
        assert_eq!(content, "alpha beta\n");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn sandbox_denies_outside_workspace_path() {
        let workspace = unique_test_dir("sandbox_workspace");
        let outside = unique_test_dir("sandbox_outside");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&outside).expect("should create outside dir");
        fs::write(outside.join("secrets.txt"), "secret").expect("should write secret");

        let err = resolve_path_for_read(
            outside.join("secrets.txt").to_string_lossy().as_ref(),
            &workspace,
            SandboxMode::Wasm,
        )
        .expect_err("outside path should be denied");
        assert!(err.to_string().contains("path denied by sandbox"));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn create_dry_run_does_not_write_file() {
        let workspace = unique_test_dir("create_dry_run");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("draft.txt");

        let out = apply_create(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "create",
                "path": "draft.txt",
                "content": "hello",
                "dry_run": true
            })),
        )
        .expect("create dry run should succeed");

        let parsed: Value = serde_json::from_str(&out).expect("output should be json");
        assert_eq!(parsed["status"], "dry_run");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(workspace);
    }
}
