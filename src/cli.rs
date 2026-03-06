use clap::{Parser, Subcommand};

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
