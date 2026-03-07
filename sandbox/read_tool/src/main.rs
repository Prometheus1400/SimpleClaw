use sandbox_common::resolve_workspace_path;
use std::env;
use std::fs;

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
