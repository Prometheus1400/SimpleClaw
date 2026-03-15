use sandbox_common::{
    EXTRA_ROOT_PREFIX, normalize_absolute_path, persona_relative_path_allowed,
};
use std::env;
use std::path::{Path, PathBuf};

use crate::config::ToolSandboxConfig;
use crate::error::{FrameworkError, SandboxCapability, SandboxPermissionDenied};
use crate::sandbox::{
    WasmPreopenedDir, normalize_workspace_root, persona_guest_mount_path,
    workspace_guest_mount_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileToolRoute {
    Sandboxed {
        guest_path: String,
        preopened_dirs: Vec<WasmPreopenedDir>,
    },
    NeedsApproval {
        capability: SandboxCapability,
        target: String,
        reason: String,
    },
}

impl FileToolRoute {
    pub fn guest_path(&self) -> Option<&str> {
        match self {
            Self::Sandboxed { guest_path, .. } => Some(guest_path),
            Self::NeedsApproval { .. } => None,
        }
    }

    pub fn preopened_dirs(&self) -> &[WasmPreopenedDir] {
        match self {
            Self::Sandboxed { preopened_dirs, .. } => preopened_dirs,
            Self::NeedsApproval { .. } => &[],
        }
    }
}

pub(crate) fn classify_wasm_tool_error(
    tool_name: &str,
    target: &str,
    capability: SandboxCapability,
    exit_code: i32,
    stderr: &str,
) -> FrameworkError {
    if is_sandbox_permission_denial(stderr) {
        return FrameworkError::sandbox_permission_denied(SandboxPermissionDenied {
            tool_name: tool_name.to_owned(),
            execution_kind: "wasm_sandbox".to_owned(),
            capability,
            target: target.to_owned(),
            diagnostic: stderr.to_owned(),
        });
    }
    FrameworkError::Tool(format!(
        "{tool_name} tool failed: exit_code={exit_code} stderr={stderr}"
    ))
}

fn all_extra_paths(sandbox: &ToolSandboxConfig) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = sandbox
        .extra_readable_paths
        .iter()
        .map(PathBuf::from)
        .collect();
    for path in &sandbox.extra_writable_paths {
        let p = PathBuf::from(path);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots
}

fn remap_guest_path_to_host(
    host_path: &Path,
    workspace_root: &Path,
    persona_root: &Path,
    sandbox: &ToolSandboxConfig,
) -> Option<PathBuf> {
    let path_str = host_path.to_str()?;

    let ws_prefix = workspace_guest_mount_path();
    if let Some(rest) = path_str.strip_prefix(ws_prefix) {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return Some(workspace_root.join(rest));
    }

    let persona_prefix = persona_guest_mount_path();
    if let Some(rest) = path_str.strip_prefix(persona_prefix) {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return Some(persona_root.join(rest));
    }

    if let Some(rest) = path_str.strip_prefix(EXTRA_ROOT_PREFIX) {
        let slash_pos = rest.find('/')?;
        let index_str = &rest[..slash_pos];
        let index: usize = index_str.parse().ok()?;
        let extra_roots = all_extra_paths(sandbox);
        let root = extra_roots.get(index)?;
        let remainder = &rest[slash_pos + 1..];
        return Some(root.join(remainder));
    }

    None
}

pub(crate) fn classify_file_tool_access(
    host_path: &Path,
    workspace_root: &Path,
    persona_root: &Path,
    sandbox: &ToolSandboxConfig,
    capability: SandboxCapability,
) -> Result<FileToolRoute, FrameworkError> {
    let host_path =
        &remap_guest_path_to_host(host_path, workspace_root, persona_root, sandbox)
            .unwrap_or_else(|| host_path.to_path_buf());
    let workspace_absolute = normalize_workspace_root(workspace_root)?;
    let normalized_workspace = normalize_absolute_path(&workspace_absolute);
    let normalized_path = normalize_absolute_path(host_path);
    if let Ok(relative) = normalized_path.strip_prefix(&normalized_workspace) {
        return Ok(FileToolRoute::Sandboxed {
            guest_path: Path::new(workspace_guest_mount_path())
                .join(relative)
                .to_string_lossy()
                .into_owned(),
            preopened_dirs: Vec::new(),
        });
    }

    let persona_absolute = normalize_workspace_root(persona_root)?;
    let normalized_persona = normalize_absolute_path(&persona_absolute);
    if let Ok(relative) = normalized_path.strip_prefix(&normalized_persona) {
        if persona_relative_path_allowed(relative) {
            return Ok(FileToolRoute::Sandboxed {
                guest_path: Path::new(persona_guest_mount_path())
                    .join(relative)
                    .to_string_lossy()
                    .into_owned(),
                preopened_dirs: Vec::new(),
            });
        }
    }

    let extra_paths = sandbox_paths_for_capability(sandbox, &capability);
    if let Some((index, extra_root, relative)) =
        find_matching_extra_root(&normalized_path, &extra_paths)?
    {
        let guest_root = format!("/__extra/{index}");
        return Ok(FileToolRoute::Sandboxed {
            guest_path: Path::new(&guest_root)
                .join(relative)
                .to_string_lossy()
                .into_owned(),
            preopened_dirs: vec![WasmPreopenedDir {
                host_path: extra_root,
                guest_path: guest_root,
            }],
        });
    }

    Ok(FileToolRoute::NeedsApproval {
        capability,
        target: normalized_path.display().to_string(),
        reason: format!(
            "target is outside the configured sandbox roots: path={} workspace={} persona={}",
            normalized_path.display(),
            normalized_workspace.display(),
            normalized_persona.display()
        ),
    })
}

pub(crate) fn resolve_path_for_read(
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

fn is_sandbox_permission_denial(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    lowered.contains("path denied by sandbox")
        || lowered.contains("permission denied")
        || lowered.contains("operation not permitted")
        || lowered.contains("access denied")
}

fn sandbox_paths_for_capability(
    sandbox: &ToolSandboxConfig,
    capability: &SandboxCapability,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in &sandbox.extra_readable_paths {
        roots.push(PathBuf::from(path));
    }
    if matches!(capability, SandboxCapability::Write) {
        for path in &sandbox.extra_writable_paths {
            roots.push(PathBuf::from(path));
        }
    }
    roots
}

fn find_matching_extra_root(
    normalized_path: &Path,
    extra_roots: &[PathBuf],
) -> Result<Option<(usize, PathBuf, PathBuf)>, FrameworkError> {
    let mut best: Option<(usize, PathBuf, PathBuf)> = None;
    let mut best_depth = 0usize;
    for (index, root) in extra_roots.iter().enumerate() {
        let normalized_root = normalize_absolute_path(&normalize_workspace_root(root)?);
        if let Ok(relative) = normalized_path.strip_prefix(&normalized_root) {
            let depth = normalized_root.components().count();
            if depth >= best_depth {
                best_depth = depth;
                best = Some((index, normalized_root, relative.to_path_buf()));
            }
        }
    }
    Ok(best)
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

    use crate::config::ToolSandboxConfig;
    use crate::error::SandboxCapability;

    use super::{FileToolRoute, classify_file_tool_access, resolve_path_for_read};

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
        let route = classify_file_tool_access(
            host_path,
            workspace,
            workspace,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("host workspace path should map to guest mount path");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. } if guest_path == "/workspace/docs/file.txt"
        ));
    }

    #[test]
    fn host_path_to_guest_path_allows_persona_prompt_file() {
        let persona = unique_test_dir("persona_prompt");
        let workspace = unique_test_dir("workspace_persona_prompt");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&persona).expect("should create persona");

        let route = classify_file_tool_access(
            &persona.join("AGENT.md"),
            &workspace,
            &persona,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("persona prompt should map");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. } if guest_path == "/persona/AGENT.md"
        ));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&persona);
    }

    #[test]
    fn host_path_to_guest_path_denies_persona_simpleclaw() {
        let persona = unique_test_dir("persona_state");
        let workspace = unique_test_dir("workspace_persona_state");
        fs::create_dir_all(persona.join(".simpleclaw")).expect("should create persona state");
        fs::create_dir_all(&workspace).expect("should create workspace");

        let route = classify_file_tool_access(
            &persona.join(".simpleclaw/memory.db"),
            &workspace,
            &persona,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("persona state should be classified");
        assert!(matches!(route, FileToolRoute::NeedsApproval { .. }));

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&persona);
    }

    #[test]
    fn host_path_maps_to_persona_guest_path() {
        let workspace = Path::new("/tmp/simpleclaw_ws");
        let persona = Path::new("/tmp/simpleclaw_persona");
        let host_path = Path::new("/tmp/simpleclaw_persona/skills/reviewer/SKILL.md");
        let route = classify_file_tool_access(
            host_path,
            workspace,
            persona,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("host persona path should map to persona guest mount path");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. }
                if guest_path == "/persona/skills/reviewer/SKILL.md"
        ));
    }

    #[test]
    fn host_path_maps_to_extra_readable_guest_path() {
        let workspace = Path::new("/tmp/simpleclaw_ws");
        let persona = Path::new("/tmp/simpleclaw_persona");
        let extra_root = Path::new("/tmp/simpleclaw_extra");
        let host_path = Path::new("/tmp/simpleclaw_extra/docs/file.txt");
        let sandbox = ToolSandboxConfig {
            extra_readable_paths: vec![extra_root.display().to_string()],
            ..ToolSandboxConfig::default()
        };

        let route = classify_file_tool_access(
            host_path,
            workspace,
            persona,
            &sandbox,
            SandboxCapability::Read,
        )
        .expect("host extra path should map to extra guest mount path");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, preopened_dirs }
                if guest_path == "/__extra/0/docs/file.txt"
                    && preopened_dirs.len() == 1
                    && preopened_dirs[0].guest_path == "/__extra/0"
        ));
    }

    #[test]
    fn guest_workspace_path_roundtrips_to_sandboxed() {
        let workspace = Path::new("/tmp/ws");
        let persona = Path::new("/tmp/persona");
        let guest_input = Path::new("/workspace/src/main.rs");

        let route = classify_file_tool_access(
            guest_input,
            workspace,
            persona,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("guest workspace path should roundtrip");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. }
                if guest_path == "/workspace/src/main.rs"
        ));
    }

    #[test]
    fn guest_persona_path_roundtrips_to_sandboxed() {
        let workspace = Path::new("/tmp/ws");
        let persona = Path::new("/tmp/persona");
        let guest_input = Path::new("/persona/AGENT.md");

        let route = classify_file_tool_access(
            guest_input,
            workspace,
            persona,
            &ToolSandboxConfig::default(),
            SandboxCapability::Read,
        )
        .expect("guest persona path should roundtrip");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. }
                if guest_path == "/persona/AGENT.md"
        ));
    }

    #[test]
    fn guest_extra_path_roundtrips_to_sandboxed() {
        let workspace = Path::new("/tmp/ws");
        let persona = Path::new("/tmp/persona");
        let extra_root = Path::new("/tmp/extra_docs");
        let guest_input = Path::new("/__extra/0/docs/file.txt");
        let sandbox = ToolSandboxConfig {
            extra_readable_paths: vec![extra_root.display().to_string()],
            ..ToolSandboxConfig::default()
        };

        let route = classify_file_tool_access(
            guest_input,
            workspace,
            persona,
            &sandbox,
            SandboxCapability::Read,
        )
        .expect("guest extra path should roundtrip");
        assert!(matches!(
            route,
            FileToolRoute::Sandboxed { guest_path, .. }
                if guest_path == "/__extra/0/docs/file.txt"
        ));
    }
}
