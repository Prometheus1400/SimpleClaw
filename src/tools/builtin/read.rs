use async_trait::async_trait;
use sandbox_common::{normalize_absolute_path, persona_relative_path_allowed};
use serde::Deserialize;
use std::fs;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ReadToolConfig;
use crate::error::FrameworkError;
use crate::sandbox::{
    RunWasmRequest, WasmSandbox, normalize_workspace_root, persona_guest_mount_path,
    workspace_guest_mount_path,
};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadTool {
    config: ReadToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub host_path: PathBuf,
    pub guest_path: Option<String>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a file or directory using JSON: {filePath, offset?, limit?}."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"filePath\":{\"type\":\"string\"},\"offset\":{\"type\":\"integer\",\"minimum\":1},\"limit\":{\"type\":\"integer\",\"minimum\":1}},\"required\":[\"filePath\"]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.read config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan)
            .await
            .map(ToolExecutionOutcome::Completed)
    }
}

impl ReadTool {
    pub fn plan(&self, ctx: &ToolExecEnv, args_json: &str) -> Result<ReadPlan, FrameworkError> {
        let args: ReadArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("read requires JSON object args: {e}")))?;
        let file_path = args.file_path.trim();
        if file_path.is_empty() {
            return Err(FrameworkError::Tool(
                "read requires a non-empty filePath".to_owned(),
            ));
        }
        let offset = args.offset.unwrap_or(1);
        if offset == 0 {
            return Err(FrameworkError::Tool(
                "read offset must be greater than or equal to 1".to_owned(),
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if limit == 0 {
            return Err(FrameworkError::Tool(
                "read limit must be greater than or equal to 1".to_owned(),
            ));
        }

        let host_path = resolve_path_for_read(file_path, &ctx.workspace_root)?;
        let guest_path =
            host_path_to_guest_path(&host_path, &ctx.workspace_root, &ctx.persona_root).ok();
        Ok(ReadPlan {
            host_path,
            guest_path,
            offset,
            limit,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv,
        plan: ReadPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let rendered = if plan.host_path.is_dir() {
            render_directory_output(&plan.host_path, plan.offset, plan.limit)?
        } else {
            render_file_output(&plan.host_path, plan.offset, plan.limit)?
        };
        Ok(ToolRunOutput::plain(rendered))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv,
        plan: ReadPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let guest_path = plan.guest_path.ok_or_else(|| {
            FrameworkError::Tool("read path is not representable inside wasm sandbox".to_owned())
        })?;
        let stdin = serde_json::to_vec(&serde_json::json!({
            "path": guest_path,
            "offset": plan.offset,
            "limit": plan.limit,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize read args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.clone(),
                persona_root: ctx.persona_root.clone(),
                artifact_name: "read_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(10)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(FrameworkError::Tool(format!(
                "read tool failed: exit_code={} stderr={}",
                output.exit_code,
                output.stderr.trim()
            )));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

fn render_file_output(path: &Path, offset: usize, limit: usize) -> Result<String, FrameworkError> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        if offset != 1 {
            return Err(FrameworkError::Tool(format!(
                "Offset {offset} is out of range for this file (0 lines)"
            )));
        }
    } else if offset > total_lines {
        return Err(FrameworkError::Tool(format!(
            "Offset {offset} is out of range for this file ({total_lines} lines)"
        )));
    }

    let start = offset.saturating_sub(1);
    let selected = lines
        .iter()
        .skip(start)
        .take(limit)
        .enumerate()
        .map(|(idx, line)| {
            let value = if line.chars().count() > MAX_LINE_LENGTH {
                format!("{}...", line.chars().take(MAX_LINE_LENGTH).collect::<String>())
            } else {
                (*line).to_owned()
            };
            format!("{}: {}", idx + offset, value)
        })
        .collect::<Vec<_>>();

    let last_read_line = if selected.is_empty() {
        offset.saturating_sub(1)
    } else {
        offset + selected.len() - 1
    };
    let has_more = last_read_line < total_lines;
    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>file</type>".to_owned(),
        "<content>".to_owned(),
    ];
    output.extend(selected);

    if has_more {
        output.push(String::new());
        output.push(format!(
            "(Showing lines {}-{} of {}. Use offset={} to continue.)",
            offset,
            last_read_line,
            total_lines,
            last_read_line + 1
        ));
    } else {
        output.push(String::new());
        output.push(format!("(End of file - total {} lines)", total_lines));
    }
    output.push("</content>".to_owned());
    Ok(output.join("\n"))
}

fn render_directory_output(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<String, FrameworkError> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| {
            let entry = entry.map_err(FrameworkError::Io)?;
            let mut name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| FrameworkError::Tool("directory entry is not utf-8".to_owned()))?
                .to_owned();
            if entry.file_type().map_err(FrameworkError::Io)?.is_dir() {
                name.push('/');
            }
            Ok(name)
        })
        .collect::<Result<Vec<_>, FrameworkError>>()?;
    entries.sort();

    if offset == 0 {
        return Err(FrameworkError::Tool(
            "read offset must be greater than or equal to 1".to_owned(),
        ));
    }
    let start = offset - 1;
    let selected = entries
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let truncated = start + selected.len() < entries.len();

    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>directory</type>".to_owned(),
        "<entries>".to_owned(),
        selected.join("\n"),
    ];
    if truncated {
        output.push(format!(
            "\n(Showing {} of {} entries. Use offset={} to continue.)",
            selected.len(),
            entries.len(),
            offset + selected.len()
        ));
    } else {
        output.push(format!("\n({} entries)", entries.len()));
    }
    output.push("</entries>".to_owned());
    Ok(output.join("\n"))
}

pub(super) fn host_path_to_guest_path(
    host_path: &Path,
    workspace_root: &Path,
    persona_root: &Path,
) -> Result<String, FrameworkError> {
    let workspace_absolute = normalize_workspace_root(workspace_root)?;
    let normalized_workspace = normalize_absolute_path(&workspace_absolute);
    let normalized_path = normalize_absolute_path(host_path);
    if let Ok(relative) = normalized_path.strip_prefix(&normalized_workspace) {
        return Ok(Path::new(workspace_guest_mount_path())
            .join(relative)
            .to_string_lossy()
            .into_owned());
    }

    let persona_absolute = normalize_workspace_root(persona_root)?;
    let normalized_persona = normalize_absolute_path(&persona_absolute);
    if let Ok(relative) = normalized_path.strip_prefix(&normalized_persona) {
        if !persona_relative_path_allowed(relative) {
            return Err(FrameworkError::Tool(format!(
                "read path denied by sandbox: path={} persona={}",
                normalized_path.display(),
                normalized_persona.display()
            )));
        }
        return Ok(Path::new(persona_guest_mount_path())
            .join(relative)
            .to_string_lossy()
            .into_owned());
    }

    Err(FrameworkError::Tool(format!(
        "read path denied by sandbox: path={} workspace={} persona={}",
        normalized_path.display(),
        normalized_workspace.display(),
        normalized_persona.display()
    )))
}

pub(super) fn resolve_path_for_read(
    raw_path: &str,
    workspace_root: &Path,
) -> Result<PathBuf, FrameworkError> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(FrameworkError::Tool(
            "read requires a non-empty path".to_owned(),
        ));
    }

    let expanded_input = expand_env_vars(trimmed);
    let expanded =
        expand_home_dir(&expanded_input).unwrap_or_else(|| PathBuf::from(expanded_input));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        workspace_root.join(expanded)
    };
    Ok(normalize_absolute_path(&absolute))
}

fn expand_home_dir(value: &str) -> Option<PathBuf> {
    if !value.starts_with('~') {
        return None;
    }
    if value.len() > 1 {
        let separator = value.as_bytes()[1];
        if separator != b'/' && separator != b'\\' {
            return None;
        }
    }

    let home = home_dir()?;
    if value == "~" {
        return Some(home);
    }

    let mut full = home;
    let remainder = &value[2..];
    if !remainder.is_empty() {
        full.push(remainder);
    }
    Some(full)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_env_vars(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < chars.len() {
        let ch = chars[i];
        if ch != '$' {
            output.push(ch);
            i += 1;
            continue;
        }

        if i + 1 >= chars.len() {
            output.push('$');
            i += 1;
            continue;
        }

        if chars[i + 1] == '{' {
            let mut j = i + 2;
            while j < chars.len() && chars[j] != '}' {
                j += 1;
            }
            if j < chars.len() {
                let key: String = chars[i + 2..j].iter().collect();
                if is_valid_env_name(&key) {
                    if let Some(val) = env::var_os(&key) {
                        output.push_str(&val.to_string_lossy());
                    }
                    i = j + 1;
                    continue;
                }
            }
            output.push('$');
            i += 1;
            continue;
        }

        let mut j = i + 1;
        while j < chars.len() && is_env_name_char(chars[j], j == i + 1) {
            j += 1;
        }
        if j == i + 1 {
            output.push('$');
            i += 1;
            continue;
        }

        let key: String = chars[i + 1..j].iter().collect();
        if let Some(val) = env::var_os(&key) {
            output.push_str(&val.to_string_lossy());
        }
        i = j;
    }

    output
}

fn is_valid_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, 'A'..='Z' | 'a'..='z' | '_') {
        return false;
    }
    chars.all(|ch| matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_'))
}

fn is_env_name_char(ch: char, first: bool) -> bool {
    if first {
        matches!(ch, 'A'..='Z' | 'a'..='z' | '_')
    } else {
        matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_')
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{host_path_to_guest_path, resolve_path_for_read};

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_read_{prefix}_{nanos}"))
    }

    #[test]
    fn resolve_path_in_wasm_sandbox_allows_relative_workspace_file() {
        let workspace = unique_test_dir("workspace_relative");
        fs::create_dir_all(&workspace).expect("should create workspace");

        let resolved =
            resolve_path_for_read("docs/file.txt", &workspace).expect("path should resolve");
        assert_eq!(resolved, workspace.join("docs/file.txt"));

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_normalizes_absolute_outside_workspace() {
        let workspace = unique_test_dir("workspace_deny_absolute");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let outside = unique_test_dir("outside_absolute").join("secrets.txt");

        let resolved = resolve_path_for_read(outside.to_string_lossy().as_ref(), &workspace)
            .expect("outside path should normalize");
        assert_eq!(resolved, outside);

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_normalizes_parent_traversal_escape() {
        let workspace = unique_test_dir("workspace_traversal");
        fs::create_dir_all(&workspace).expect("should create workspace");

        let resolved = resolve_path_for_read("../outside.txt", &workspace)
            .expect("parent traversal should normalize");
        assert_eq!(
            resolved,
            workspace
                .parent()
                .expect("workspace parent")
                .join("outside.txt")
        );

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_allows_absolute_outside_workspace() {
        let workspace = unique_test_dir("workspace_off");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let outside = unique_test_dir("outside_off").join("secrets.txt");

        let resolved = resolve_path_for_read(outside.to_string_lossy().as_ref(), &workspace)
            .expect("outside path should resolve");
        assert_eq!(resolved, outside);

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_expands_home_prefix() {
        let workspace = unique_test_dir("workspace_home");
        let fake_home = unique_test_dir("fake_home");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&fake_home).expect("should create fake home");
        unsafe {
            std::env::set_var("HOME", &fake_home);
        }

        let resolved =
            resolve_path_for_read("~/keys.txt", &workspace).expect("home path should resolve");
        assert_eq!(resolved, fake_home.join("keys.txt"));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn resolve_path_expands_environment_variable() {
        let workspace = unique_test_dir("workspace_env");
        let env_root = unique_test_dir("env_root");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&env_root).expect("should create env root");
        unsafe {
            std::env::set_var("SIMPLECLAW_READ_TEST_DIR", &env_root);
        }

        let resolved = resolve_path_for_read("$SIMPLECLAW_READ_TEST_DIR/token.txt", &workspace)
            .expect("env path should resolve");
        assert_eq!(resolved, env_root.join("token.txt"));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&env_root);
    }

    #[test]
    fn host_path_maps_to_workspace_guest_path() {
        let workspace = Path::new("/tmp/simpleclaw_ws");
        let host_path = Path::new("/tmp/simpleclaw_ws/docs/file.txt");
        let guest_path = host_path_to_guest_path(host_path, workspace, workspace)
            .expect("host workspace path should map to guest mount path");
        assert_eq!(guest_path, "/workspace/docs/file.txt");
    }

    #[test]
    fn host_path_to_guest_path_allows_persona_prompt_file() {
        let persona = unique_test_dir("persona_prompt");
        let workspace = unique_test_dir("workspace_persona_prompt");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&persona).expect("should create persona");

        let guest_path = host_path_to_guest_path(&persona.join("AGENT.md"), &workspace, &persona)
            .expect("persona prompt should map");
        assert_eq!(guest_path, "/persona/AGENT.md");

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&persona);
    }

    #[test]
    fn host_path_to_guest_path_denies_persona_simpleclaw() {
        let persona = unique_test_dir("persona_state");
        let workspace = unique_test_dir("workspace_persona_state");
        fs::create_dir_all(persona.join(".simpleclaw")).expect("should create persona state");
        fs::create_dir_all(&workspace).expect("should create workspace");

        let err =
            host_path_to_guest_path(&persona.join(".simpleclaw/memory.db"), &workspace, &persona)
                .expect_err("persona state should not be representable");
        assert!(err.to_string().contains("read path denied by sandbox"));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&persona);
    }

    #[test]
    fn host_path_maps_to_persona_guest_path() {
        let workspace = Path::new("/tmp/simpleclaw_ws");
        let persona = Path::new("/tmp/simpleclaw_persona");
        let host_path = Path::new("/tmp/simpleclaw_persona/skills/reviewer/SKILL.md");
        let guest_path = host_path_to_guest_path(host_path, workspace, persona)
            .expect("host persona path should map to persona guest mount path");
        assert_eq!(guest_path, "/persona/skills/reviewer/SKILL.md");
    }
}
