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

    use super::{AgentAction, Cli, Command, MemoryMode};

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
}
