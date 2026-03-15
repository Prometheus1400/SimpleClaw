use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Component, Path, PathBuf};

const PREVIEW_CHARS: usize = 1_000;
pub const WORKSPACE_ROOT: &str = "/workspace";
pub const PERSONA_ROOT: &str = "/persona";
pub const EXTRA_ROOT_PREFIX: &str = "/__extra/";
const PROMPT_FILES: &[&str] = &["IDENTITY.md", "AGENT.md", "USER.md", "MEMORY.md", "SOUL.md"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditArgs {
    pub command: String,
    pub path: String,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub line: Option<usize>,
    #[serde(default)]
    pub replace_all: bool,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| !matches!(last, Component::RootDir | Component::Prefix(_)));
                if can_pop {
                    normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub fn resolve_guest_path(raw_path: &str) -> Result<PathBuf, String> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err("path must be non-empty".to_owned());
    }

    let input = PathBuf::from(trimmed);
    let absolute = if input.is_absolute() {
        input
    } else {
        Path::new(WORKSPACE_ROOT).join(input)
    };

    let normalized = normalize_absolute_path(&absolute);
    if normalized.starts_with(Path::new(WORKSPACE_ROOT)) {
        return Ok(normalized);
    }

    if normalized.starts_with(Path::new(PERSONA_ROOT)) && guest_persona_path_allowed(&normalized) {
        return Ok(normalized);
    }

    if let Some(rest) = normalized
        .to_str()
        .and_then(|s| s.strip_prefix(EXTRA_ROOT_PREFIX))
    {
        let index_end = rest.find('/').unwrap_or(rest.len());
        let index_str = &rest[..index_end];
        if !index_str.is_empty() && index_str.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(normalized);
        }
    }

    Err(format!(
        "path denied by sandbox: path={} workspace={} persona={}",
        normalized.display(),
        WORKSPACE_ROOT,
        PERSONA_ROOT
    ))
}

pub fn guest_persona_path_allowed(path: &Path) -> bool {
    let normalized = normalize_absolute_path(path);
    let persona_root = Path::new(PERSONA_ROOT);
    let Ok(relative) = normalized.strip_prefix(persona_root) else {
        return false;
    };
    persona_relative_path_allowed(relative)
}

pub fn persona_relative_path_allowed(relative: &Path) -> bool {
    let mut components = relative.components();
    match components.next() {
        None => false,
        Some(Component::Normal(first)) if first == ".simpleclaw" => false,
        Some(Component::Normal(first)) if first == "skills" => true,
        Some(Component::Normal(first)) => {
            components.next().is_none() && PROMPT_FILES.iter().any(|file| first == *file)
        }
        _ => false,
    }
}

pub fn apply_edit_command(args: &EditArgs) -> Result<String, String> {
    let path = resolve_guest_path(&args.path)?;
    apply_edit_command_at_path(args, &path)
}

pub fn apply_edit_command_at_path(args: &EditArgs, path: &Path) -> Result<String, String> {
    let command = args.command.trim();
    if command.is_empty() {
        return Err("edit requires a non-empty command".to_owned());
    }
    if args.path.trim().is_empty() {
        return Err("edit requires a non-empty path".to_owned());
    }

    let path_display = path.display().to_string();
    match command {
        "create" => apply_create(path, &path_display, args),
        "replace" => apply_replace(path, &path_display, args),
        "insert" => apply_insert(path, &path_display, args),
        "delete" => apply_delete(path, &path_display, args),
        "append" => apply_append(path, &path_display, args),
        other => Err(format!(
            "edit command must be one of create|replace|insert|delete|append (got: {other})"
        )),
    }
}

pub fn apply_create(path: &Path, path_display: &str, args: &EditArgs) -> Result<String, String> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| "edit create requires content".to_owned())?;
    let exists = path.exists();
    if exists && !args.overwrite {
        return Err(format!(
            "edit create refused because file exists: {path_display} (set overwrite=true to replace)"
        ));
    }
    if !args.dry_run {
        fs::write(path, content).map_err(|e| format!("failed to write file: {e}"))?;
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

pub fn apply_replace(path: &Path, path_display: &str, args: &EditArgs) -> Result<String, String> {
    let old_text = args
        .old_text
        .as_ref()
        .ok_or_else(|| "edit replace requires old_text".to_owned())?;
    let new_text = args
        .new_text
        .as_ref()
        .ok_or_else(|| "edit replace requires new_text".to_owned())?;
    if old_text.is_empty() {
        return Err("edit replace requires non-empty old_text".to_owned());
    }

    let original =
        fs::read_to_string(path).map_err(|e| format!("failed to read {path_display}: {e}"))?;
    let occurrences = original.match_indices(old_text).count();
    if occurrences == 0 {
        return Err(format!(
            "edit replace found no matches for old_text in {path_display}"
        ));
    }
    if occurrences > 1 && !args.replace_all {
        return Err(format!(
            "edit replace found {occurrences} matches in {path_display}; set replace_all=true to replace all"
        ));
    }

    let updated = if args.replace_all {
        original.replace(old_text, new_text)
    } else {
        original.replacen(old_text, new_text, 1)
    };
    if !args.dry_run {
        fs::write(path, &updated).map_err(|e| format!("failed to write file: {e}"))?;
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

pub fn apply_insert(path: &Path, path_display: &str, args: &EditArgs) -> Result<String, String> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| "edit insert requires content".to_owned())?;
    let line = args
        .line
        .ok_or_else(|| "edit insert requires line".to_owned())?;
    if line == 0 {
        return Err("edit insert line must be >= 1".to_owned());
    }

    let original =
        fs::read_to_string(path).map_err(|e| format!("failed to read {path_display}: {e}"))?;
    let idx = byte_index_for_line(&original, line)
        .ok_or_else(|| format!("edit insert line {line} out of range for {path_display}"))?;
    let mut updated = String::with_capacity(original.len() + content.len());
    updated.push_str(&original[..idx]);
    updated.push_str(content);
    updated.push_str(&original[idx..]);

    if !args.dry_run {
        fs::write(path, &updated).map_err(|e| format!("failed to write file: {e}"))?;
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

pub fn apply_delete(path: &Path, path_display: &str, args: &EditArgs) -> Result<String, String> {
    let old_text = args
        .old_text
        .as_ref()
        .ok_or_else(|| "edit delete requires old_text".to_owned())?;
    if old_text.is_empty() {
        return Err("edit delete requires non-empty old_text".to_owned());
    }

    let original =
        fs::read_to_string(path).map_err(|e| format!("failed to read {path_display}: {e}"))?;
    let occurrences = original.match_indices(old_text).count();
    if occurrences == 0 {
        return Err(format!(
            "edit delete found no matches for old_text in {path_display}"
        ));
    }
    if occurrences > 1 && !args.replace_all {
        return Err(format!(
            "edit delete found {occurrences} matches in {path_display}; set replace_all=true to delete all"
        ));
    }

    let updated = if args.replace_all {
        original.replace(old_text, "")
    } else {
        original.replacen(old_text, "", 1)
    };
    if !args.dry_run {
        fs::write(path, &updated).map_err(|e| format!("failed to write file: {e}"))?;
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

pub fn apply_append(path: &Path, path_display: &str, args: &EditArgs) -> Result<String, String> {
    let content = args
        .content
        .as_ref()
        .ok_or_else(|| "edit append requires content".to_owned())?;

    let original =
        fs::read_to_string(path).map_err(|e| format!("failed to read {path_display}: {e}"))?;
    let mut updated = String::with_capacity(original.len() + content.len());
    updated.push_str(&original);
    updated.push_str(content);
    if !args.dry_run {
        fs::write(path, &updated).map_err(|e| format!("failed to write file: {e}"))?;
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

pub fn json_result(
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

pub fn truncate_for_preview(text: &str) -> String {
    if text.chars().count() <= PREVIEW_CHARS {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(PREVIEW_CHARS).collect();
    format!("{prefix}...<truncated>")
}

pub fn byte_index_for_line(content: &str, line: usize) -> Option<usize> {
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
    use super::{guest_persona_path_allowed, persona_relative_path_allowed, resolve_guest_path};
    use std::path::Path;

    #[test]
    fn resolve_guest_path_allows_workspace_relative_paths() {
        let resolved = resolve_guest_path("docs/file.txt").expect("workspace path should resolve");
        assert_eq!(resolved, Path::new("/workspace/docs/file.txt"));
    }

    #[test]
    fn resolve_guest_path_allows_prompt_files_in_persona() {
        let resolved = resolve_guest_path("/persona/AGENT.md").expect("prompt file should resolve");
        assert_eq!(resolved, Path::new("/persona/AGENT.md"));
    }

    #[test]
    fn resolve_guest_path_allows_persona_skills_tree() {
        let resolved = resolve_guest_path("/persona/skills/reviewer/SKILL.md")
            .expect("skill path should resolve");
        assert_eq!(resolved, Path::new("/persona/skills/reviewer/SKILL.md"));
    }

    #[test]
    fn resolve_guest_path_denies_persona_simpleclaw() {
        let err = resolve_guest_path("/persona/.simpleclaw/memory/short.db")
            .expect_err("persona state should be denied");
        assert!(err.contains("path denied by sandbox"));
    }

    #[test]
    fn resolve_guest_path_denies_other_persona_files() {
        let err = resolve_guest_path("/persona/notes.txt")
            .expect_err("non-prompt persona file should be denied");
        assert!(err.contains("path denied by sandbox"));
    }

    #[test]
    fn persona_relative_path_allowed_matches_expected_surface() {
        assert!(persona_relative_path_allowed(Path::new("AGENT.md")));
        assert!(persona_relative_path_allowed(Path::new(
            "skills/reviewer/SKILL.md"
        )));
        assert!(!persona_relative_path_allowed(Path::new(
            ".simpleclaw/memory/short.db"
        )));
        assert!(!persona_relative_path_allowed(Path::new("notes.txt")));
        assert!(!persona_relative_path_allowed(Path::new("nested/AGENT.md")));
    }

    #[test]
    fn resolve_guest_path_allows_extra_with_numeric_index() {
        let resolved =
            resolve_guest_path("/__extra/0/docs/file.txt").expect("extra path should resolve");
        assert_eq!(resolved, Path::new("/__extra/0/docs/file.txt"));
    }

    #[test]
    fn resolve_guest_path_allows_extra_with_higher_index() {
        let resolved =
            resolve_guest_path("/__extra/2/nested/path.rs").expect("extra path should resolve");
        assert_eq!(resolved, Path::new("/__extra/2/nested/path.rs"));
    }

    #[test]
    fn resolve_guest_path_denies_extra_with_non_numeric_index() {
        let err = resolve_guest_path("/__extra/notanum/file.txt")
            .expect_err("non-numeric index should be denied");
        assert!(err.contains("path denied by sandbox"));
    }

    #[test]
    fn resolve_guest_path_denies_extra_prefix_lookalike() {
        let err = resolve_guest_path("/__extrafoo/0/file.txt")
            .expect_err("lookalike prefix should be denied");
        assert!(err.contains("path denied by sandbox"));
    }

    #[test]
    fn guest_persona_path_allowed_requires_persona_prefix() {
        assert!(guest_persona_path_allowed(Path::new("/persona/USER.md")));
        assert!(!guest_persona_path_allowed(Path::new("/workspace/USER.md")));
    }
}
