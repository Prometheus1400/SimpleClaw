use glob::Pattern;
use regex::Regex;
use sandbox_common::resolve_guest_path;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_LINE_LENGTH: usize = 2000;
const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    path: String,
    include: Option<String>,
}

#[derive(Debug)]
struct GrepMatch {
    path: PathBuf,
    line_number: usize,
    line_text: String,
    modified: SystemTime,
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
    let args: GrepArgs =
        serde_json::from_str(&input).map_err(|e| format!("grep requires JSON object args: {e}"))?;
    if args.pattern.trim().is_empty() {
        return Err("grep requires a non-empty pattern".to_owned());
    }
    let root = resolve_guest_path(&args.path)?;
    if !root.is_dir() {
        return Err(format!("grep path must be a directory: {}", root.display()));
    }

    let regex = Regex::new(args.pattern.trim())
        .map_err(|e| format!("grep pattern is invalid regex: {e}"))?;
    let include = args
        .include
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|e| format!("grep include pattern is invalid: {e}"))?;

    let files = collect_files(&root)?;
    let mut matches = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .map_err(|e| format!("grep failed to compute relative path: {e}"))?;
        if let Some(include_pattern) = include.as_ref() {
            if !include_pattern.matches_path(relative) {
                continue;
            }
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let modified = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                let line_text = if line.chars().count() > MAX_LINE_LENGTH {
                    format!(
                        "{}...",
                        line.chars().take(MAX_LINE_LENGTH).collect::<String>()
                    )
                } else {
                    line.to_owned()
                };
                matches.push(GrepMatch {
                    path: path.clone(),
                    line_number: idx + 1,
                    line_text,
                    modified,
                });
            }
        }
    }

    if matches.is_empty() {
        print!("No files found");
        return Ok(());
    }
    matches.sort_by(|a, b| b.modified.cmp(&a.modified));
    let total = matches.len();
    let truncated = total > LIMIT;
    let selected = matches.into_iter().take(LIMIT).collect::<Vec<_>>();
    let mut output = vec![if truncated {
        format!("Found {total} matches (showing first {LIMIT})")
    } else {
        format!("Found {total} matches")
    }];

    let mut current: Option<PathBuf> = None;
    for item in selected {
        if current.as_ref() != Some(&item.path) {
            if current.is_some() {
                output.push(String::new());
            }
            current = Some(item.path.clone());
            output.push(format!("{}:", item.path.display()));
        }
        output.push(format!("  Line {}: {}", item.line_number, item.line_text));
    }

    print!("{}", output.join("\n"));
    Ok(())
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("grep failed to read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("grep failed to walk directory: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("grep failed to inspect {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}
