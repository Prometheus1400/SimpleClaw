//! Ripgrep integration for dangerous file discovery.

use std::path::Path;
use std::process::Command;

use crate::config::{RipgrepConfig, DANGEROUS_DIRECTORIES, DANGEROUS_FILES};
use crate::error::SandboxError;

/// Default search depth for mandatory deny discovery.
pub const DEFAULT_SEARCH_DEPTH: u32 = 3;

/// Find dangerous files in a directory using ripgrep.
/// Returns a list of absolute paths to dangerous files/directories.
pub fn find_dangerous_files(
    cwd: &Path,
    config: Option<&RipgrepConfig>,
    max_depth: Option<u32>,
) -> Result<Vec<String>, SandboxError> {
    let rg_config = config.cloned().unwrap_or_default();
    let depth = max_depth.unwrap_or(DEFAULT_SEARCH_DEPTH);

    let mut cmd = Command::new(&rg_config.command);

    // Basic flags
    cmd.arg("--files")
        .arg("--hidden")
        .arg("--max-depth")
        .arg(depth.to_string());

    // Add iglob patterns for dangerous files (case-insensitive)
    for file in DANGEROUS_FILES {
        cmd.arg("--iglob").arg(format!("**/{}", file));
    }

    // Add glob patterns for dangerous directories
    for dir in DANGEROUS_DIRECTORIES {
        cmd.arg("--iglob").arg(format!("**/{}/**", dir));
    }

    // Exclude node_modules to speed up search
    cmd.arg("-g").arg("!**/node_modules/**");

    // Add any custom args
    if let Some(ref args) = rg_config.args {
        for arg in args {
            cmd.arg(arg);
        }
    }

    // Set the working directory
    cmd.arg(cwd);

    // Execute the command
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SandboxError::MissingDependency(format!(
                "ripgrep not found at '{}'. Please install ripgrep.",
                rg_config.command
            ))
        } else {
            SandboxError::Io(e)
        }
    })?;

    // Parse the output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            // Ensure paths are absolute
            let path = Path::new(line);
            if path.is_absolute() {
                line.to_string()
            } else {
                cwd.join(line).display().to_string()
            }
        })
        .collect();

    Ok(files)
}

/// Check if ripgrep is available.
pub fn check_ripgrep(config: Option<&RipgrepConfig>) -> bool {
    let command = config.map(|c| c.command.as_str()).unwrap_or("rg");

    Command::new(command)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_ripgrep() {
        // This test will pass if ripgrep is installed
        let available = check_ripgrep(None);
        // We don't assert the result since it depends on the environment
        println!("Ripgrep available: {}", available);
    }
}
