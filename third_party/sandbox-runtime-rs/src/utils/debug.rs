//! Debug logging utilities.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::EnvFilter;

/// Global debug flag.
static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Environment variable for debug mode.
pub const SRT_DEBUG_ENV: &str = "SRT_DEBUG";

/// Initialize debug logging based on the SRT_DEBUG environment variable or explicit flag.
pub fn init_debug_logging(force_debug: bool) {
    let debug_enabled = force_debug || std::env::var(SRT_DEBUG_ENV).is_ok();
    DEBUG_ENABLED.store(debug_enabled, Ordering::SeqCst);

    let filter = if debug_enabled {
        EnvFilter::new("sandbox_runtime=debug,warn")
    } else {
        EnvFilter::new("sandbox_runtime=info,warn")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(debug_enabled)
        .with_ansi(true)
        .try_init()
        .ok();
}

/// Check if debug mode is enabled.
pub fn is_debug_enabled() -> bool {
    DEBUG_ENABLED.load(Ordering::SeqCst)
}

/// Log a debug message (only if debug is enabled).
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::utils::debug::is_debug_enabled() {
            tracing::debug!($($arg)*);
        }
    };
}

/// Log a verbose debug message.
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        if $crate::utils::debug::is_debug_enabled() {
            tracing::trace!($($arg)*);
        }
    };
}
