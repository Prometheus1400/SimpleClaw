mod agent;
mod channel;
mod cli;
mod config;
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

use crate::cli::{Cli, Command, ListAction, SystemAction};
use crate::config::ProviderKind;

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
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
