use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

fn main() {
    if let Err(msg) = run() {
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let raw_path = env::args()
        .nth(1)
        .ok_or_else(|| "missing path argument".to_owned())?;
    let path = resolve_workspace_path(&raw_path)?;
    let content =
        fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    print!("{content}");
    Ok(())
}

fn resolve_workspace_path(raw_path: &str) -> Result<PathBuf, String> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err("path must be non-empty".to_owned());
    }

    let input = PathBuf::from(trimmed);
    let absolute = if input.is_absolute() {
        input
    } else {
        Path::new("/workspace").join(input)
    };

    let normalized = normalize_absolute_path(&absolute);
    let workspace_root = Path::new("/workspace");
    if !normalized.starts_with(workspace_root) {
        return Err(format!(
            "path denied by sandbox: path={} workspace={}",
            normalized.display(),
            workspace_root.display()
        ));
    }
    Ok(normalized)
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
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
