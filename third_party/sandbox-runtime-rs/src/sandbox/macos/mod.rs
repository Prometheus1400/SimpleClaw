//! macOS sandbox implementation using Seatbelt/sandbox-exec.

pub mod glob;
pub mod monitor;
pub mod profile;
pub mod wrapper;

pub use monitor::LogMonitor;
pub use profile::{generate_log_tag, generate_profile};
pub use wrapper::{cleanup_temp_profiles, generate_proxy_env, wrap_command};
