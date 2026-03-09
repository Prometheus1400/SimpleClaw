//! Utility modules.

pub mod debug;
pub mod path;
pub mod platform;
pub mod ripgrep;
pub mod shell;

pub use debug::{init_debug_logging, is_debug_enabled, SRT_DEBUG_ENV};
pub use path::{
    contains_glob_chars, expand_home, is_symlink_outside_boundary, normalize_case_for_comparison,
    normalize_path_for_sandbox, remove_trailing_glob_suffix,
};
pub use platform::{current_platform, get_arch, get_wsl_version, is_linux, is_macos, Platform};
pub use ripgrep::{check_ripgrep, find_dangerous_files};
pub use shell::{join_args, quote, split_args};
