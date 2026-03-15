use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "SimpleClaw",
    version,
    about = "Lightweight Rust Agentic Framework"
)]
pub struct Cli {
    #[arg(long)]
    pub workspace: Option<std::path::PathBuf>,

    #[arg(long, default_value_t = 8)]
    pub max_steps: u32,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[cfg(feature = "audio")]
    Audio {
        #[command(subcommand)]
        action: AudioAction,
    },
    System {
        #[command(subcommand)]
        action: SystemAction,
    },
    Logs {
        #[arg(long)]
        follow: bool,
    },
    Status,
    Providers {
        #[command(subcommand)]
        action: ListAction,
    },
    Models {
        #[command(subcommand)]
        action: ListAction,
    },
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Debug, Subcommand, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    Run,
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Subcommand, Clone, Copy, PartialEq, Eq)]
pub enum ListAction {
    List,
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Memory {
        #[arg(long)]
        agent: String,
        #[arg(long, value_enum)]
        memory: MemoryMode,
        #[arg(long, value_parser = parse_limit)]
        limit: usize,
    },
}

#[cfg(feature = "audio")]
#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum AudioAction {
    Status,
    List,
    Install {
        #[command(subcommand)]
        target: AudioInstallTarget,
    },
}

#[cfg(feature = "audio")]
#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum AudioInstallTarget {
    Whisper {
        #[arg(long)]
        force: bool,
    },
    Piper {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum AuthAction {
    Login {
        #[arg(long, value_enum)]
        provider: AuthProvider,
        #[arg(long)]
        profile: Option<String>,
    },
    Status {
        #[arg(long, value_enum)]
        provider: AuthProvider,
    },
    Logout {
        #[arg(long, value_enum)]
        provider: AuthProvider,
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum AuthProvider {
    #[value(name = "openai_codex")]
    OpenaiCodex,
}

impl AuthProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCodex => "openai_codex",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum MemoryMode {
    Short,
    Long,
    Both,
}

fn parse_limit(raw: &str) -> Result<usize, String> {
    let value: usize = raw
        .parse()
        .map_err(|_| format!("invalid integer value '{raw}'"))?;
    if value == 0 {
        return Err("limit must be greater than 0".to_owned());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{AgentAction, AuthAction, AuthProvider, Cli, Command, MemoryMode};
    #[cfg(feature = "audio")]
    use super::{AudioAction, AudioInstallTarget};

    #[test]
    fn parses_agent_memory_short() {
        let cli = Cli::parse_from([
            "simpleclaw",
            "agent",
            "memory",
            "--agent",
            "planner",
            "--memory",
            "short",
            "--limit",
            "5",
        ]);

        match cli.command {
            Some(Command::Agent {
                action:
                    AgentAction::Memory {
                        agent,
                        memory,
                        limit,
                    },
            }) => {
                assert_eq!(agent, "planner");
                assert_eq!(memory, MemoryMode::Short);
                assert_eq!(limit, 5);
            }
            _ => panic!("expected agent memory command"),
        }
    }

    #[test]
    fn parses_agent_memory_long() {
        let cli = Cli::parse_from([
            "simpleclaw",
            "agent",
            "memory",
            "--agent",
            "planner",
            "--memory",
            "long",
            "--limit",
            "7",
        ]);

        match cli.command {
            Some(Command::Agent {
                action:
                    AgentAction::Memory {
                        agent,
                        memory,
                        limit,
                    },
            }) => {
                assert_eq!(agent, "planner");
                assert_eq!(memory, MemoryMode::Long);
                assert_eq!(limit, 7);
            }
            _ => panic!("expected agent memory command"),
        }
    }

    #[test]
    fn parses_agent_memory_both() {
        let cli = Cli::parse_from([
            "simpleclaw",
            "agent",
            "memory",
            "--agent",
            "planner",
            "--memory",
            "both",
            "--limit",
            "11",
        ]);

        match cli.command {
            Some(Command::Agent {
                action:
                    AgentAction::Memory {
                        agent,
                        memory,
                        limit,
                    },
            }) => {
                assert_eq!(agent, "planner");
                assert_eq!(memory, MemoryMode::Both);
                assert_eq!(limit, 11);
            }
            _ => panic!("expected agent memory command"),
        }
    }

    #[test]
    fn rejects_invalid_memory_mode() {
        let result = Cli::try_parse_from([
            "simpleclaw",
            "agent",
            "memory",
            "--agent",
            "planner",
            "--memory",
            "invalid",
            "--limit",
            "5",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_missing_required_flags() {
        let result = Cli::try_parse_from(["simpleclaw", "agent", "memory", "--agent", "planner"]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_non_positive_limit() {
        let result = Cli::try_parse_from([
            "simpleclaw",
            "agent",
            "memory",
            "--agent",
            "planner",
            "--memory",
            "short",
            "--limit",
            "0",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_auth_login_defaults_profile_to_none() {
        let cli = Cli::parse_from(["simpleclaw", "auth", "login", "--provider", "openai_codex"]);

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Login { provider, profile },
            }) => {
                assert_eq!(provider, AuthProvider::OpenaiCodex);
                assert!(profile.is_none());
            }
            _ => panic!("expected auth login command"),
        }
    }

    #[test]
    fn parses_auth_logout_with_profile() {
        let cli = Cli::parse_from([
            "simpleclaw",
            "auth",
            "logout",
            "--provider",
            "openai_codex",
            "--profile",
            "work",
        ]);

        match cli.command {
            Some(Command::Auth {
                action: AuthAction::Logout { provider, profile },
            }) => {
                assert_eq!(provider, AuthProvider::OpenaiCodex);
                assert_eq!(profile.as_deref(), Some("work"));
            }
            _ => panic!("expected auth logout command"),
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn parses_audio_status() {
        let cli = Cli::parse_from(["simpleclaw", "audio", "status"]);

        match cli.command {
            Some(Command::Audio {
                action: AudioAction::Status,
            }) => {}
            _ => panic!("expected audio status command"),
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn parses_audio_install_whisper_force() {
        let cli = Cli::parse_from(["simpleclaw", "audio", "install", "whisper", "--force"]);

        match cli.command {
            Some(Command::Audio {
                action:
                    AudioAction::Install {
                        target: AudioInstallTarget::Whisper { force },
                    },
            }) => assert!(force),
            _ => panic!("expected audio whisper install command"),
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn parses_audio_install_piper() {
        let cli = Cli::parse_from(["simpleclaw", "audio", "install", "piper"]);

        match cli.command {
            Some(Command::Audio {
                action:
                    AudioAction::Install {
                        target: AudioInstallTarget::Piper { force },
                    },
            }) => assert!(!force),
            _ => panic!("expected audio piper install command"),
        }
    }

    #[cfg(not(feature = "audio"))]
    #[test]
    fn rejects_audio_command_without_feature() {
        let result = Cli::try_parse_from(["simpleclaw", "audio", "status"]);
        assert!(result.is_err());
    }
}
