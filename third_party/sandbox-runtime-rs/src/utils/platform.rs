//! Platform detection utilities.

/// Supported platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Linux,
}

impl Platform {
    /// Detect the current platform.
    /// Note: All Linux including WSL returns Linux. Use `get_wsl_version()` to detect WSL1 (unsupported).
    pub fn current() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            Some(Platform::MacOS)
        }
        #[cfg(target_os = "linux")]
        {
            // WSL2+ is treated as Linux (same sandboxing)
            // WSL1 is also returned as Linux but will fail is_supported() check
            Some(Platform::Linux)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    /// Check if the current platform is supported.
    /// Returns false for unsupported platforms and WSL1.
    pub fn is_supported() -> bool {
        match Self::current() {
            Some(Platform::Linux) => {
                // WSL1 doesn't support bubblewrap
                get_wsl_version() != Some("1".to_string())
            }
            Some(Platform::MacOS) => true,
            None => false,
        }
    }

    /// Get the platform name as a string.
    pub fn name(&self) -> &'static str {
        match self {
            Platform::MacOS => "macOS",
            Platform::Linux => "Linux",
        }
    }
}

/// Get the current platform, if supported.
pub fn current_platform() -> Option<Platform> {
    Platform::current()
}

/// Check if running on macOS.
#[inline]
pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// Check if running on Linux.
#[inline]
pub fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// Get the CPU architecture.
pub fn get_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        "unknown"
    }
}

/// Get the WSL version (1 or 2+) if running in WSL.
/// Returns None if not running in WSL.
///
/// Detection logic:
/// 1. Read /proc/version which contains kernel info
/// 2. Look for explicit "WSL2", "WSL3" etc. markers (case-insensitive)
/// 3. If no explicit version but "microsoft" is present, assume WSL1
///    (handles the original WSL1 format like "4.4.0-19041-Microsoft")
///
/// WSL1 is unsupported because bubblewrap requires user namespaces which WSL1 lacks.
/// WSL2 runs a real Linux kernel and supports full sandboxing.
pub fn get_wsl_version() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;

        let proc_version = match fs::read_to_string("/proc/version") {
            Ok(content) => content,
            Err(_) => return None,
        };

        parse_wsl_version_from_string(&proc_version)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Parse WSL version from a /proc/version string.
/// Extracted for unit testing.
#[cfg(any(target_os = "linux", test))]
fn parse_wsl_version_from_string(proc_version: &str) -> Option<String> {
    // Check for explicit WSL version markers (e.g., "WSL2", "WSL3", etc.)
    // Use a simple pattern match since we can't use regex easily here
    let proc_lower = proc_version.to_lowercase();

    // Look for "wsl" followed by a digit - use proc_lower for both finding and extraction
    // to ensure consistent byte positions
    if let Some(pos) = proc_lower.find("wsl") {
        let after_wsl = &proc_lower[pos + 3..];
        if let Some(ch) = after_wsl.chars().next() {
            if ch.is_ascii_digit() {
                return Some(ch.to_string());
            }
        }
    }

    // If no explicit WSL version but contains Microsoft, assume WSL1
    // This handles the original WSL1 format: "4.4.0-19041-Microsoft"
    if proc_lower.contains("microsoft") {
        return Some("1".to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_arch() {
        let arch = get_arch();
        assert!(arch == "x64" || arch == "arm64" || arch == "unknown");
    }

    #[test]
    fn test_platform_current() {
        let platform = Platform::current();
        #[cfg(target_os = "macos")]
        assert_eq!(platform, Some(Platform::MacOS));
        #[cfg(target_os = "linux")]
        assert_eq!(platform, Some(Platform::Linux));
    }

    #[test]
    fn test_get_wsl_version_non_linux() {
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(get_wsl_version(), None);
        }
    }

    #[test]
    fn test_wsl_version_parsing_wsl2() {
        // WSL2 kernel version string (typical format)
        let wsl2_version = "Linux version 5.15.90.1-microsoft-standard-WSL2 (oe-user@oe-host)";
        assert_eq!(parse_wsl_version_from_string(wsl2_version), Some("2".to_string()));

        // Case insensitivity
        let wsl2_upper = "Linux version 5.15.90.1-MICROSOFT-STANDARD-WSL2";
        assert_eq!(parse_wsl_version_from_string(wsl2_upper), Some("2".to_string()));
    }

    #[test]
    fn test_wsl_version_parsing_wsl1() {
        // WSL1 kernel version string (original format with just "Microsoft")
        let wsl1_version = "Linux version 4.4.0-19041-Microsoft (Microsoft@Microsoft.com)";
        assert_eq!(parse_wsl_version_from_string(wsl1_version), Some("1".to_string()));

        // Case variations
        let wsl1_lower = "linux version 4.4.0-19041-microsoft";
        assert_eq!(parse_wsl_version_from_string(wsl1_lower), Some("1".to_string()));
    }

    #[test]
    fn test_wsl_version_parsing_native_linux() {
        // Native Linux (no WSL markers)
        let native = "Linux version 6.2.0-26-generic (buildd@ubuntu)";
        assert_eq!(parse_wsl_version_from_string(native), None);

        // Empty string
        assert_eq!(parse_wsl_version_from_string(""), None);
    }

    #[test]
    fn test_wsl_version_parsing_future_version() {
        // Future WSL versions (WSL3, WSL4, etc.)
        let wsl3 = "Linux version 6.0.0-microsoft-standard-WSL3";
        assert_eq!(parse_wsl_version_from_string(wsl3), Some("3".to_string()));

        let wsl9 = "Linux version 7.0.0-microsoft-standard-WSL9";
        assert_eq!(parse_wsl_version_from_string(wsl9), Some("9".to_string()));
    }
}
