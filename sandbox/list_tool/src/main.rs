use glob::Pattern;
use sandbox_common::{resolve_guest_path, WORKSPACE_ROOT};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const LIMIT: usize = 100;
const IGNORE_PATTERNS: &[&str] = &[
    "node_modules/**",
    "__pycache__/**",
    ".git/**",
    "dist/**",
    "build/**",
    "target/**",
    "vendor/**",
    "bin/**",
    "obj/**",
    ".idea/**",
    ".vscode/**",
    ".cache/**",
    "cache/**",
    "logs/**",
    ".venv/**",
    "venv/**",
    "env/**",
];

#[derive(Debug, Deserialize)]
struct ListArgs {
    path: String,
    ignore: Option<Vec<String>>,
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed reading stdin: {e}"))?;
    let args: ListArgs =
        serde_json::from_str(&input).map_err(|e| format!("list requires JSON object args: {e}"))?;
    let root = resolve_guest_path(&args.path)?;
    if !root.is_dir() {
        return Err(format!("list path must be a directory: {}", root.display()));
    }
    let output = run_list(
        &root,
        &display_root(&root)?,
        args.ignore.unwrap_or_default(),
    )?;
    print!("{output}");
    Ok(())
}

fn run_list(root: &Path, display_root: &str, ignore: Vec<String>) -> Result<String, String> {
    let mut ignore_patterns = Vec::new();
    for pattern in IGNORE_PATTERNS {
        ignore_patterns.push(
            Pattern::new(pattern).map_err(|e| format!("invalid built-in ignore pattern: {e}"))?,
        );
    }
    for pattern in ignore {
        ignore_patterns.push(
            Pattern::new(&pattern)
                .map_err(|e| format!("invalid ignore pattern '{pattern}': {e}"))?,
        );
    }

    let mut dirs = BTreeSet::new();
    let mut files_by_dir: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    dirs.insert(PathBuf::from("."));

    let mut files_seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("list failed to read {}: {e}", dir.display()))?;
        for entry in entries {
            if files_seen >= LIMIT {
                break;
            }
            let entry = entry.map_err(|e| format!("list failed to walk directory: {e}"))?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| format!("list failed to compute relative path: {e}"))?;
            if is_ignored(relative, &ignore_patterns) {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|e| format!("list failed to inspect {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path.clone());
                dirs.insert(relative.to_path_buf());
                let mut parent = relative.parent();
                while let Some(next) = parent {
                    dirs.insert(if next.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        next.to_path_buf()
                    });
                    parent = next.parent();
                }
            } else if file_type.is_file() {
                files_seen += 1;
                let parent = normalize_dir_key(relative.parent()).to_path_buf();
                let name = relative
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or_default()
                    .to_owned();
                files_by_dir.entry(parent).or_default().push(name);
            }
        }
    }

    let mut output = String::new();
    output.push_str(&format!("{display_root}\n"));
    output.push_str(&render_dir(Path::new("."), 0, &dirs, &files_by_dir));
    Ok(output)
}

fn display_root(root: &Path) -> Result<String, String> {
    let workspace_root = Path::new(WORKSPACE_ROOT);
    let relative = root
        .strip_prefix(workspace_root)
        .map_err(|_| format!("list path is outside workspace mount: {}", root.display()))?;
    if relative.as_os_str().is_empty() {
        Ok("./".to_owned())
    } else {
        Ok(format!("{}/", relative.display()))
    }
}

fn render_dir(
    dir: &Path,
    depth: usize,
    dirs: &BTreeSet<PathBuf>,
    files_by_dir: &BTreeMap<PathBuf, Vec<String>>,
) -> String {
    let mut output = String::new();
    if depth > 0 {
        let indent = "  ".repeat(depth);
        output.push_str(&format!(
            "{}{}/\n",
            indent,
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("")
        ));
    }

    let children = dirs
        .iter()
        .filter(|candidate| {
            candidate != &&PathBuf::from(".") && normalize_dir_key(candidate.parent()) == dir
        })
        .cloned()
        .collect::<Vec<_>>();

    for child in children {
        output.push_str(&render_dir(&child, depth + 1, dirs, files_by_dir));
    }

    let dir_key = if dir == Path::new(".") {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    };
    if let Some(files) = files_by_dir.get(&dir_key) {
        let indent = "  ".repeat(depth + 1);
        let mut files = files.clone();
        files.sort();
        for file in files {
            output.push_str(&format!("{}{}\n", indent, file));
        }
    }

    output
}

fn normalize_dir_key(path: Option<&Path>) -> &Path {
    match path {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn is_ignored(path: &Path, patterns: &[Pattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches_path(path))
}

#[cfg(test)]
mod tests {
    use super::{display_root, run_list};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_sandbox_list_{prefix}_{nanos}"))
    }

    #[test]
    fn run_list_renders_workspace_root_as_dot_slash() {
        let workspace = unique_test_dir("workspace_root");
        fs::create_dir_all(workspace.join("docs")).expect("should create docs dir");
        fs::write(workspace.join("docs/file.txt"), "hello").expect("should write file");

        let output = run_list(&workspace, "./", Vec::new()).expect("list should succeed");

        assert!(output.starts_with("./\n"), "output={output}");
        assert!(output.contains("  docs/\n"), "output={output}");
        assert!(output.contains("    file.txt\n"), "output={output}");
        assert!(!output.contains("/workspace/"), "output={output}");

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn run_list_renders_nested_path_relative_to_workspace() {
        let workspace = unique_test_dir("nested_root");
        let nested = workspace.join("docs");
        fs::create_dir_all(&nested).expect("should create nested dir");
        fs::write(nested.join("file.txt"), "hello").expect("should write file");
        let output = run_list(&nested, "docs/", Vec::new()).expect("list should succeed");

        assert!(output.starts_with("docs/\n"), "output={output}");
        assert!(output.contains("  file.txt\n"), "output={output}");
        assert!(!output.contains("/workspace/docs"), "output={output}");

        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn display_root_uses_workspace_relative_labels() {
        assert_eq!(
            display_root(Path::new("/workspace")).expect("workspace root should render"),
            "./"
        );
        assert_eq!(
            display_root(Path::new("/workspace/docs")).expect("nested root should render"),
            "docs/"
        );
    }

    #[test]
    fn display_root_rejects_paths_outside_workspace_mount() {
        let err = display_root(Path::new("/persona")).expect_err("non-workspace path should fail");
        assert!(err.contains("outside workspace mount"));
    }
}
