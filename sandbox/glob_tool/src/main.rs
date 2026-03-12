use glob::Pattern;
use sandbox_common::resolve_guest_path;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    path: String,
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
    let args: GlobArgs =
        serde_json::from_str(&input).map_err(|e| format!("glob requires JSON object args: {e}"))?;
    if args.pattern.trim().is_empty() {
        return Err("glob requires a non-empty pattern".to_owned());
    }
    let root = resolve_guest_path(&args.path)?;
    if !root.is_dir() {
        return Err(format!("glob path must be a directory: {}", root.display()));
    }
    let pattern =
        Pattern::new(args.pattern.trim()).map_err(|e| format!("glob pattern is invalid: {e}"))?;
    let mut stack = vec![root.clone()];
    let mut matches: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("glob failed to read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("glob failed to walk directory: {e}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("glob failed to inspect {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path.clone());
            }
            let relative = path
                .strip_prefix(&root)
                .map_err(|e| format!("glob failed to compute relative path: {e}"))?;
            if pattern.matches_path(relative) {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                matches.push((path, modified));
            }
        }
    }

    matches.sort_by(|a, b| b.1.cmp(&a.1));
    if matches.is_empty() {
        print!("No files found");
        return Ok(());
    }
    let output = matches
        .into_iter()
        .take(LIMIT)
        .map(|(path, _)| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    print!("{output}");
    Ok(())
}
