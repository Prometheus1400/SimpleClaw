use glob::Pattern;
use sandbox_common::resolve_guest_path;
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
    let output = run_list(&root, args.ignore.unwrap_or_default())?;
    print!("{output}");
    Ok(())
}

fn run_list(root: &Path, ignore: Vec<String>) -> Result<String, String> {
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
                let parent = relative
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
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
    output.push_str(&format!("{}/\n", root.display()));
    output.push_str(&render_dir(Path::new("."), 0, &dirs, &files_by_dir));
    Ok(output)
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
            candidate != &&PathBuf::from(".")
                && candidate.parent().unwrap_or_else(|| Path::new(".")) == dir
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

fn is_ignored(path: &Path, patterns: &[Pattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches_path(path))
}
