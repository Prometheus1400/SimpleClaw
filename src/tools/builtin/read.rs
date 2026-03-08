use async_trait::async_trait;
use sandbox_common::normalize_absolute_path;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::FrameworkError;
use crate::tools::sandbox::{normalize_workspace_root, run_wasm_guest, workspace_guest_mount_path};
use crate::tools::{Tool, ToolExecEnv};

use super::common::parse_simple_text_arg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTool {
    LocalFile,
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

    fn sandbox_aware(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<String, FrameworkError> {
        let raw_path = parse_simple_text_arg(args_json);
        let path = resolve_path_for_read(&raw_path, &ctx.workspace_root, ctx.sandbox.enabled)?;
        if ctx.sandbox.enabled {
            let guest_path = host_path_to_workspace_guest_path(&path, &ctx.workspace_root)?;
            let output = run_wasm_guest(
                &ctx.workspace_root,
                "read_tool.wasm",
                &[guest_path],
                &[],
                Duration::from_secs(10),
            )
            .await?;
            if output.exit_code != 0 {
                return Err(FrameworkError::Tool(format!(
                    "read tool failed: exit_code={} stderr={}",
                    output.exit_code,
                    output.stderr.trim()
                )));
            }
            return Ok(output.stdout);
        }
        let content = std::fs::read_to_string(path)?;
        Ok(content)
    }
}

fn host_path_to_workspace_guest_path(
    host_path: &Path,
    workspace_root: &Path,
) -> Result<String, FrameworkError> {
    let workspace_absolute = normalize_workspace_root(workspace_root)?;
    let normalized_workspace = normalize_absolute_path(&workspace_absolute);
    let normalized_path = normalize_absolute_path(host_path);
    let relative = normalized_path
        .strip_prefix(&normalized_workspace)
        .map_err(|_| {
            FrameworkError::Tool(format!(
                "read path denied by sandbox: path={} workspace={}",
                normalized_path.display(),
                normalized_workspace.display()
            ))
        })?;
    let guest_path = Path::new(workspace_guest_mount_path()).join(relative);
    Ok(guest_path.to_string_lossy().into_owned())
}

pub(super) fn resolve_path_for_read(
    raw_path: &str,
    workspace_root: &Path,
    sandbox_enabled: bool,
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
    let normalized_path = normalize_absolute_path(&absolute);

    if sandbox_enabled {
        let workspace_absolute = if workspace_root.is_absolute() {
            workspace_root.to_path_buf()
        } else {
            env::current_dir()
                .map_err(FrameworkError::Io)?
                .join(workspace_root)
        };
        let normalized_workspace = normalize_absolute_path(&workspace_absolute);
        if !normalized_path.starts_with(&normalized_workspace) {
            return Err(FrameworkError::Tool(format!(
                "read path denied by sandbox: path={} workspace={}",
                normalized_path.display(),
                normalized_workspace.display()
            )));
        }
    }

    Ok(normalized_path)
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

    use super::{host_path_to_workspace_guest_path, resolve_path_for_read};

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

        let resolved = resolve_path_for_read("docs/file.txt", &workspace, true)
            .expect("path should resolve");
        assert_eq!(resolved, workspace.join("docs/file.txt"));

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_in_wasm_sandbox_denies_absolute_outside_workspace() {
        let workspace = unique_test_dir("workspace_deny_absolute");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let outside = unique_test_dir("outside_absolute").join("secrets.txt");

        let err = resolve_path_for_read(
            outside.to_string_lossy().as_ref(),
            &workspace,
            true,
        )
        .expect_err("outside path should be denied");
        assert!(err.to_string().contains("read path denied by sandbox"));

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_in_wasm_sandbox_denies_parent_traversal_escape() {
        let workspace = unique_test_dir("workspace_traversal");
        fs::create_dir_all(&workspace).expect("should create workspace");

        let err = resolve_path_for_read("../outside.txt", &workspace, true)
            .expect_err("parent traversal should be denied");
        assert!(err.to_string().contains("read path denied by sandbox"));

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_path_with_sandbox_off_allows_absolute_outside_workspace() {
        let workspace = unique_test_dir("workspace_off");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let outside = unique_test_dir("outside_off").join("secrets.txt");

        let resolved = resolve_path_for_read(
            outside.to_string_lossy().as_ref(),
            &workspace,
            false,
        )
        .expect("outside path should resolve with sandbox off");
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

        let resolved = resolve_path_for_read("~/keys.txt", &workspace, false)
            .expect("home path should resolve");
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

        let resolved = resolve_path_for_read(
            "$SIMPLECLAW_READ_TEST_DIR/token.txt",
            &workspace,
            false,
        )
        .expect("env path should resolve");
        assert_eq!(resolved, env_root.join("token.txt"));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&env_root);
    }

    #[test]
    fn host_path_maps_to_workspace_guest_path() {
        let workspace = Path::new("/tmp/simpleclaw_ws");
        let host_path = Path::new("/tmp/simpleclaw_ws/docs/file.txt");
        let guest_path = host_path_to_workspace_guest_path(host_path, workspace)
            .expect("host workspace path should map to guest mount path");
        assert_eq!(guest_path, "/workspace/docs/file.txt");
    }
}
