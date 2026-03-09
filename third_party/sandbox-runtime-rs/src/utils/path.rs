//! Path normalization utilities.

use std::path::{Path, PathBuf};

/// Normalize a path for sandbox use.
/// - Expands ~ to home directory
/// - Resolves to canonical path if possible
/// - Returns the normalized path string
pub fn normalize_path_for_sandbox(path: &str) -> String {
    let expanded = expand_home(path);

    // Try to canonicalize (resolves symlinks)
    match std::fs::canonicalize(&expanded) {
        Ok(canonical) => canonical.display().to_string(),
        Err(_) => expanded,
    }
}

/// Expand ~ to the home directory.
pub fn expand_home(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.display(), &path[1..]);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.display().to_string();
        }
    }
    path.to_string()
}

/// Normalize case for path comparison on case-insensitive filesystems.
pub fn normalize_case_for_comparison(path: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        path.to_lowercase()
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.to_string()
    }
}

/// Check if a path contains glob characters.
pub fn contains_glob_chars(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('[') || path.contains('{')
}

/// Remove trailing glob suffix (e.g., /** or /*)
pub fn remove_trailing_glob_suffix(path: &str) -> String {
    let mut result = path.to_string();

    // Remove trailing /**
    while result.ends_with("/**") {
        result = result[..result.len() - 3].to_string();
    }

    // Remove trailing /*
    while result.ends_with("/*") {
        result = result[..result.len() - 2].to_string();
    }

    result
}

/// Check if a resolved symlink path is outside the original path boundary.
/// This prevents escaping the sandbox via symlinks.
pub fn is_symlink_outside_boundary(original: &Path, resolved: &Path) -> bool {
    // If the resolved path is an ancestor of or equal to root, it's outside
    if resolved == Path::new("/") {
        return true;
    }

    // Check if resolved is an ancestor of original
    if original.starts_with(resolved) && original != resolved {
        return true;
    }

    false
}

/// Get the parent directory path, handling root correctly.
pub fn get_parent_path(path: &Path) -> Option<&Path> {
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        None
    } else {
        Some(parent)
    }
}

/// Join paths, handling absolute paths correctly.
pub fn join_paths<P: AsRef<Path>>(base: &Path, path: P) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Check if a path is a symlink.
pub fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Resolve a symlink to its target, if it is one.
pub fn resolve_symlink(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::read_link(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_home() {
        let home = dirs::home_dir().unwrap();

        assert_eq!(expand_home("~"), home.display().to_string());
        assert_eq!(
            expand_home("~/Documents"),
            format!("{}/Documents", home.display())
        );
        assert_eq!(expand_home("/absolute/path"), "/absolute/path");
        assert_eq!(expand_home("relative/path"), "relative/path");
    }

    #[test]
    fn test_contains_glob_chars() {
        assert!(contains_glob_chars("*.txt"));
        assert!(contains_glob_chars("src/**/*.rs"));
        assert!(contains_glob_chars("file?.txt"));
        assert!(contains_glob_chars("file[0-9].txt"));
        assert!(contains_glob_chars("file{a,b}.txt"));
        assert!(!contains_glob_chars("/plain/path"));
    }

    #[test]
    fn test_remove_trailing_glob_suffix() {
        assert_eq!(remove_trailing_glob_suffix("/path/**"), "/path");
        assert_eq!(remove_trailing_glob_suffix("/path/*"), "/path");
        assert_eq!(remove_trailing_glob_suffix("/path/**/**"), "/path");
        assert_eq!(remove_trailing_glob_suffix("/path"), "/path");
    }
}
