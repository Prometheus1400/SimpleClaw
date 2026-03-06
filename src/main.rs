#![deny(warnings)]

mod agent;
mod channel;
mod cli;
mod config;
mod dispatch;
mod error;
mod gateway;
mod memory;
mod paths;
mod prompt;
mod provider;
mod react;
mod run;
mod secrets;
mod tools;

use clap::Parser;
use color_eyre::eyre::WrapErr;

use crate::cli::{AgentAction, Cli, Command, ListAction, SystemAction};
use crate::config::{GlobalConfig, LogLevel, ProviderKind};
use crate::paths::AppPaths;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    init_tracing();

    let cli = Cli::parse();
    match &cli.command {
        Some(Command::System {
            action: SystemAction::Run,
        }) => run::run_service(&cli).await?,
        Some(Command::System {
            action: SystemAction::Start,
        }) => run::start_service(&cli)?,
        Some(Command::System {
            action: SystemAction::Stop,
        }) => run::stop_service(&cli)?,
        Some(Command::System {
            action: SystemAction::Restart,
        }) => {
            run::stop_service(&cli)?;
            run::start_service(&cli)?;
        }
        Some(Command::Logs { follow }) => run::show_logs(&cli, *follow)?,
        Some(Command::Status) => run::show_status(&cli)?,
        Some(Command::Providers {
            action: ListAction::List,
        }) => list_providers(&cli)?,
        Some(Command::Models {
            action: ListAction::List,
        }) => list_models(&cli)?,
        Some(Command::Agent {
            action:
                AgentAction::Memory {
                    agent,
                    memory,
                    limit,
                },
        }) => run::show_agent_memory(&cli, agent, *memory, *limit).await?,
        None => run::run_service(&cli).await?,
    }
    Ok(())
}

fn list_providers(cli: &Cli) -> color_eyre::Result<()> {
    let loaded = crate::config::LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load configuration for provider listing")?;
    let configured = loaded.global.provider.kind;
    for provider in known_providers() {
        let label = provider.as_str();
        if *provider == configured {
            println!("{label} (configured)");
        } else {
            println!("{label}");
        }
    }
    Ok(())
}

fn list_models(cli: &Cli) -> color_eyre::Result<()> {
    let loaded = crate::config::LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load configuration for model listing")?;
    let provider = loaded.global.provider.kind;
    let configured_model = loaded.global.provider.model;
    let mut showed_configured = false;

    for model in known_models_for(provider) {
        if *model == configured_model {
            println!("{model} (configured)");
            showed_configured = true;
        } else {
            println!("{model}");
        }
    }

    if !showed_configured {
        println!("{configured_model} (configured custom)");
    }
    Ok(())
}

fn known_providers() -> &'static [ProviderKind] {
    &[ProviderKind::Gemini]
}

fn known_models_for(provider: ProviderKind) -> &'static [&'static str] {
    match provider {
        ProviderKind::Gemini => &[
            "gemini-2.5-flash",
            "gemini-2.5-pro",
            "gemini-2.0-flash",
            "gemini-2.0-flash-lite",
        ],
    }
}

fn init_tracing() {
    use std::str::FromStr;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::{Directive, LevelFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let configured = load_configured_log_level();
        let app_level = LevelFilter::from_str(configured.as_str()).unwrap_or(LevelFilter::INFO);
        let dep_level = if configured == LogLevel::Trace {
            LevelFilter::TRACE
        } else {
            LevelFilter::WARN
        };

        let mut filter = EnvFilter::default()
            .add_directive(LevelFilter::INFO.into())
            .add_directive(
                Directive::from_str(&format!("{}={app_level}", env!("CARGO_CRATE_NAME")))
                    .expect("crate directive should be valid"),
            );

        for target in [
            "serenity",
            "reqwest",
            "hyper",
            "hyper_util",
            "h2",
            "tokio_tungstenite",
            "tungstenite",
            "rustls",
        ] {
            filter = filter.add_directive(
                Directive::from_str(&format!("{target}={dep_level}"))
                    .expect("dependency directive should be valid"),
            );
        }

        filter
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn load_configured_log_level() -> LogLevel {
    let Ok(paths) = AppPaths::resolve() else {
        return LogLevel::default();
    };
    if !paths.config_path.exists() {
        return LogLevel::default();
    }

    let content = match std::fs::read_to_string(&paths.config_path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!(
                "warning: failed to read {} for runtime.log_level: {err}",
                paths.config_path.display()
            );
            return LogLevel::default();
        }
    };

    match serde_yaml::from_str::<GlobalConfig>(&content) {
        Ok(global) => global.runtime.log_level,
        Err(err) => {
            eprintln!(
                "warning: failed to parse {} for runtime.log_level: {err}",
                paths.config_path.display()
            );
            LogLevel::default()
        }
    }
}
