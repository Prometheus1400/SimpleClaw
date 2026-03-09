//! Filesystem configuration processing.

use std::path::{Path, PathBuf};

use crate::config::FilesystemConfig;
use crate::utils::{contains_glob_chars, normalize_path_for_sandbox};

/// Processed filesystem read restriction configuration.
#[derive(Debug, Clone, Default)]
pub struct FsReadRestrictionConfig {
    /// Paths denied for reading.
    pub deny_paths: Vec<PathBuf>,
    /// Glob patterns denied for reading.
    pub deny_patterns: Vec<String>,
}

/// Processed filesystem write restriction configuration.
#[derive(Debug, Clone, Default)]
pub struct FsWriteRestrictionConfig {
    /// Paths allowed for writing.
    pub allow_paths: Vec<PathBuf>,
    /// Glob patterns allowed for writing.
    pub allow_patterns: Vec<String>,
    /// Paths denied for writing (overrides allow).
    pub deny_paths: Vec<PathBuf>,
    /// Glob patterns denied for writing.
    pub deny_patterns: Vec<String>,
}

/// Process filesystem configuration into normalized paths.
pub fn process_fs_config(config: &FilesystemConfig) -> (FsReadRestrictionConfig, FsWriteRestrictionConfig) {
    let mut read_config = FsReadRestrictionConfig::default();
    let mut write_config = FsWriteRestrictionConfig::default();

    // Process deny_read
    for path in &config.deny_read {
        let normalized = normalize_path_for_sandbox(path);
        if contains_glob_chars(&normalized) {
            read_config.deny_patterns.push(normalized);
        } else {
            read_config.deny_paths.push(PathBuf::from(normalized));
        }
    }

    // Process allow_write
    for path in &config.allow_write {
        let normalized = normalize_path_for_sandbox(path);
        if contains_glob_chars(&normalized) {
            write_config.allow_patterns.push(normalized);
        } else {
            write_config.allow_paths.push(PathBuf::from(normalized));
        }
    }

    // Process deny_write
    for path in &config.deny_write {
        let normalized = normalize_path_for_sandbox(path);
        if contains_glob_chars(&normalized) {
            write_config.deny_patterns.push(normalized);
        } else {
            write_config.deny_paths.push(PathBuf::from(normalized));
        }
    }

    (read_config, write_config)
}

/// Check if a path is within any of the allowed paths.
pub fn is_path_allowed(path: &Path, allowed_paths: &[PathBuf]) -> bool {
    for allowed in allowed_paths {
        if path.starts_with(allowed) {
            return true;
        }
    }
    false
}

/// Check if a path is denied.
pub fn is_path_denied(path: &Path, denied_paths: &[PathBuf]) -> bool {
    for denied in denied_paths {
        if path.starts_with(denied) || path == denied.as_path() {
            return true;
        }
    }
    false
}
