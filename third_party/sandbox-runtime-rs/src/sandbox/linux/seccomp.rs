//! Seccomp filter loading and management.

use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::config::SeccompConfig;
use crate::error::SandboxError;
use crate::utils::get_arch;

/// Cache for BPF path lookups (key: explicit path or empty string, value: resolved path or None)
static BPF_PATH_CACHE: Lazy<Mutex<std::collections::HashMap<String, Option<PathBuf>>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Cache for apply-seccomp binary path lookups
static APPLY_SECCOMP_PATH_CACHE: Lazy<Mutex<std::collections::HashMap<String, Option<PathBuf>>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Get local paths to check for seccomp files (bundled or package installs).
fn get_local_seccomp_paths(filename: &str) -> Vec<PathBuf> {
    let arch = get_arch();
    if arch == "unknown" {
        return vec![];
    }

    let mut paths = vec![
        // Relative to working directory
        PathBuf::from(format!("vendor/seccomp/{}/{}", arch, filename)),
        // Relative to executable
        PathBuf::from(format!("../vendor/seccomp/{}/{}", arch, filename)),
        // Embedded location (relative to crate root)
        PathBuf::from(format!("../../vendor/seccomp/{}/{}", arch, filename)),
    ];

    // Also try relative to current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            paths.push(exe_dir.join(format!("vendor/seccomp/{}/{}", arch, filename)));
            paths.push(exe_dir.join(format!("../vendor/seccomp/{}/{}", arch, filename)));
            paths.push(exe_dir.join(format!("../../vendor/seccomp/{}/{}", arch, filename)));
        }
    }

    // Fallback: check cargo install location
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(format!(".cargo/share/sandbox-runtime/seccomp/{}/{}", arch, filename)));
    }

    paths
}

/// Find the BPF path without caching (internal implementation).
fn find_bpf_path(explicit_path: Option<&str>) -> Option<PathBuf> {
    // Check explicit path first (highest priority)
    if let Some(path_str) = explicit_path {
        let p = PathBuf::from(path_str);
        if p.exists() {
            tracing::debug!("[SeccompFilter] Using BPF filter from explicit path: {:?}", p);
            return Some(p);
        }
        tracing::debug!(
            "[SeccompFilter] Explicit path provided but file not found: {}",
            path_str
        );
    }

    let arch = get_arch();
    if arch == "unknown" {
        tracing::debug!(
            "[SeccompFilter] Unsupported architecture for pre-generated BPF: {}",
            std::env::consts::ARCH
        );
        return None;
    }

    // Check local paths (bundled or package install)
    for path in get_local_seccomp_paths("unix-block.bpf") {
        if path.exists() {
            tracing::debug!(
                "[SeccompFilter] Found pre-generated BPF filter: {:?} ({})",
                path,
                arch
            );
            return Some(path);
        }
    }

    tracing::debug!(
        "[SeccompFilter] Pre-generated BPF filter not found in any expected location ({})",
        arch
    );
    None
}

/// Find the apply-seccomp binary path without caching (internal implementation).
fn find_apply_seccomp_path(explicit_path: Option<&str>) -> Option<PathBuf> {
    // Check explicit path first (highest priority)
    if let Some(path_str) = explicit_path {
        let p = PathBuf::from(path_str);
        if p.exists() {
            tracing::debug!(
                "[SeccompFilter] Using apply-seccomp binary from explicit path: {:?}",
                p
            );
            return Some(p);
        }
        tracing::debug!(
            "[SeccompFilter] Explicit path provided but file not found: {}",
            path_str
        );
    }

    let arch = get_arch();
    if arch == "unknown" {
        tracing::debug!(
            "[SeccompFilter] Unsupported architecture for apply-seccomp: {}",
            std::env::consts::ARCH
        );
        return None;
    }

    // Check local paths (bundled or package install)
    for path in get_local_seccomp_paths("apply-seccomp") {
        if path.exists() {
            tracing::debug!(
                "[SeccompFilter] Found apply-seccomp binary: {:?} ({})",
                path,
                arch
            );
            return Some(path);
        }
    }

    tracing::debug!(
        "[SeccompFilter] apply-seccomp binary not found in any expected location ({})",
        arch
    );
    None
}

/// Get the path to the seccomp BPF filter for the current architecture.
/// Results are cached for performance.
pub fn get_bpf_path(config: Option<&SeccompConfig>) -> Result<PathBuf, SandboxError> {
    let explicit_path = config.and_then(|c| c.bpf_path.as_deref());
    let cache_key = explicit_path.unwrap_or("").to_string();

    // Check cache first
    {
        let cache = BPF_PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone().ok_or_else(|| {
                SandboxError::Seccomp(format!(
                    "Could not find seccomp BPF filter for architecture '{}'",
                    get_arch()
                ))
            });
        }
    }

    // Find path and cache result
    let result = find_bpf_path(explicit_path);
    {
        let mut cache = BPF_PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(cache_key, result.clone());
    }

    result.ok_or_else(|| {
        SandboxError::Seccomp(format!(
            "Could not find seccomp BPF filter for architecture '{}'",
            get_arch()
        ))
    })
}

/// Get the path to the apply-seccomp binary for the current architecture.
/// Results are cached for performance.
pub fn get_apply_seccomp_path(config: Option<&SeccompConfig>) -> Result<PathBuf, SandboxError> {
    let explicit_path = config.and_then(|c| c.apply_path.as_deref());
    let cache_key = explicit_path.unwrap_or("").to_string();

    // Check cache first
    {
        let cache = APPLY_SECCOMP_PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone().ok_or_else(|| {
                SandboxError::Seccomp(format!(
                    "Could not find apply-seccomp binary for architecture '{}'",
                    get_arch()
                ))
            });
        }
    }

    // Find path and cache result
    let result = find_apply_seccomp_path(explicit_path);
    {
        let mut cache = APPLY_SECCOMP_PATH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(cache_key, result.clone());
    }

    result.ok_or_else(|| {
        SandboxError::Seccomp(format!(
            "Could not find apply-seccomp binary for architecture '{}'",
            get_arch()
        ))
    })
}

/// Check if seccomp is available on the current system.
pub fn is_seccomp_available(config: Option<&SeccompConfig>) -> bool {
    get_bpf_path(config).is_ok() && get_apply_seccomp_path(config).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_arch() {
        let arch = get_arch();
        assert!(arch == "x64" || arch == "arm64" || arch == "unknown");
    }
}
