//! CLI parsing and execution.

use std::path::PathBuf;

use clap::Parser;

/// Sandbox Runtime - OS-level sandboxing tool
#[derive(Parser, Debug)]
#[command(name = "srt")]
#[command(about = "Sandbox Runtime - enforce filesystem and network restrictions on processes")]
#[command(version)]
pub struct Cli {
    /// Enable debug logging
    #[arg(short = 'd', long = "debug")]
    pub debug: bool,

    /// Path to settings file (default: ~/.srt-settings.json)
    #[arg(short = 's', long = "settings")]
    pub settings: Option<PathBuf>,

    /// Run command string directly (sh -c mode)
    #[arg(short = 'c')]
    pub command: Option<String>,

    /// Read config updates from file descriptor (JSON lines protocol)
    #[arg(long = "control-fd")]
    pub control_fd: Option<i32>,

    /// Command and arguments to run
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

impl Cli {
    /// Parse CLI arguments.
    pub fn parse_args() -> Self {
        Cli::parse()
    }

    /// Get the command to execute.
    /// Returns (command_string, shell_mode)
    /// - shell_mode = true when using -c flag
    /// - shell_mode = false when using positional args
    pub fn get_command(&self) -> Option<(String, bool)> {
        if let Some(ref cmd) = self.command {
            Some((cmd.clone(), true))
        } else if !self.args.is_empty() {
            // Join args with proper quoting
            let cmd = crate::utils::join_args(&self.args);
            Some((cmd, false))
        } else {
            None
        }
    }

    /// Get the settings file path.
    pub fn get_settings_path(&self) -> Option<PathBuf> {
        self.settings.clone().or_else(crate::config::default_settings_path)
    }
}
