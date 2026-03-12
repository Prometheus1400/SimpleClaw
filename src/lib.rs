//! SimpleClaw agent framework.
#![deny(warnings)]
#![deny(missing_docs)]

mod agent;
mod auth;
mod channels;
mod cli;
mod config;
mod dispatch;
mod error;
mod gateway;
mod invoke;
mod memory;
mod paths;
mod prompt;
mod providers;
mod react;
mod reply_policy;
mod run;
pub(crate) mod sandbox;
mod secrets;
mod telemetry;
/// Public test harness helpers for black-box integration tests.
pub mod testing;
mod tools;

use clap::Parser;
use color_eyre::eyre::WrapErr;
use std::str::FromStr;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{Directive, LevelFilter};
use tracing_subscriber::prelude::*;

use crate::cli::{AgentAction, AuthAction, AuthProvider, Cli, Command, ListAction, SystemAction};
use crate::config::{GlobalConfig, LogLevel};
use crate::paths::AppPaths;
use crate::providers::ProviderRegistry;

/// Run the CLI application: parse args, initialize tracing, and dispatch commands.
pub async fn run() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    init_tracing(&cli)?;

    match &cli.command {
        Some(Command::System {
            action: SystemAction::Run,
        }) => run::run_service().await?,
        Some(Command::System {
            action: SystemAction::Start,
        }) => run::start_service()?,
        Some(Command::System {
            action: SystemAction::Stop,
        }) => run::stop_service(&cli)?,
        Some(Command::System {
            action: SystemAction::Restart,
        }) => {
            run::stop_service(&cli)?;
            run::start_service()?;
        }
        Some(Command::Logs { follow }) => run::show_logs(&cli, *follow)?,
        Some(Command::Status) => run::show_status(&cli)?,
        Some(Command::Providers {
            action: ListAction::List,
        }) => list_providers(&cli)?,
        Some(Command::Models {
            action: ListAction::List,
        }) => list_models(&cli)?,
        Some(Command::Auth { action }) => run_auth_action(action).await?,
        Some(Command::Agent {
            action:
                AgentAction::Memory {
                    agent,
                    memory,
                    limit,
                },
        }) => run::show_agent_memory(&cli, agent, *memory, *limit).await?,
        None => run::run_service().await?,
    }
    Ok(())
}

async fn run_auth_action(action: &AuthAction) -> color_eyre::Result<()> {
    let service = auth::AuthService::new_default()?;

    match action {
        AuthAction::Login { provider, profile } => match provider {
            AuthProvider::OpenaiCodex => {
                let profile_name = profile
                    .as_deref()
                    .unwrap_or(auth::AuthService::default_profile_name());
                service.login_openai_codex(profile_name).await?;
                println!(
                    "login complete for provider '{}' (profile '{}')",
                    provider.as_str(),
                    profile_name
                );
            }
        },
        AuthAction::Status { provider } => match provider {
            AuthProvider::OpenaiCodex => {
                let (active_profile, profiles) = service.status_openai_codex().await?;
                if profiles.is_empty() {
                    println!(
                        "no auth profiles found for provider '{}'",
                        provider.as_str()
                    );
                    return Ok(());
                }
                let active = active_profile.as_deref();
                println!("provider: {}", provider.as_str());
                for profile in profiles {
                    let marker = if active == Some(profile.id.as_str()) {
                        " (active)"
                    } else {
                        ""
                    };
                    println!(
                        "- {}{} | account_id={} | expires_at={}",
                        profile.profile_name,
                        marker,
                        profile.account_id.as_deref().unwrap_or("unknown"),
                        auth::format_expires_at(profile.token_set.expires_at_unix),
                    );
                }
            }
        },
        AuthAction::Logout { provider, profile } => match provider {
            AuthProvider::OpenaiCodex => {
                let removed = service.logout_openai_codex(profile.as_deref()).await?;
                if removed {
                    let profile_name = profile.as_deref().unwrap_or("(active)");
                    println!(
                        "logged out provider '{}' profile {}",
                        provider.as_str(),
                        profile_name
                    );
                } else {
                    println!(
                        "no matching profile found for provider '{}'",
                        provider.as_str()
                    );
                }
            }
        },
    }

    Ok(())
}

fn list_providers(cli: &Cli) -> color_eyre::Result<()> {
    let loaded = crate::config::LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load configuration for provider listing")?;
    let mut entries = loaded
        .global
        .providers
        .entries
        .iter()
        .collect::<Vec<(&String, &crate::config::ProviderEntryConfig)>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (key, entry) in entries {
        let kind = entry.kind().as_str();
        if *key == loaded.global.providers.default {
            println!("{key} ({kind}, default)");
        } else {
            println!("{key} ({kind})");
        }
    }
    Ok(())
}

fn list_models(cli: &Cli) -> color_eyre::Result<()> {
    let loaded = crate::config::LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load configuration for model listing")?;
    let registry = ProviderRegistry::new();
    let mut entries = loaded
        .global
        .providers
        .entries
        .iter()
        .collect::<Vec<(&String, &crate::config::ProviderEntryConfig)>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (index, (key, entry)) in entries.into_iter().enumerate() {
        if index > 0 {
            println!();
        }
        let metadata = registry
            .metadata_for_kind(entry.kind())
            .wrap_err("failed to resolve provider metadata")?;
        let configured_model = entry.model();
        println!("{key} ({}):", metadata.kind.as_str());

        let mut showed_configured = false;
        for model in metadata.known_models {
            if *model == configured_model {
                println!("  {model} (configured)");
                showed_configured = true;
            } else {
                println!("  {model}");
            }
        }

        if !showed_configured {
            println!("  {configured_model} (configured custom)");
        }
    }
    Ok(())
}

fn init_tracing(cli: &Cli) -> color_eyre::Result<()> {
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

    let log_to_file = matches!(
        cli.command,
        Some(Command::System {
            action: SystemAction::Run
        }) | None
    );
    if log_to_file {
        let paths = AppPaths::resolve().wrap_err("failed to resolve runtime paths for logging")?;
        paths
            .ensure_runtime_dirs()
            .wrap_err("failed to create runtime directories for logging")?;
        let writer = crate::run::RotatingLogWriter::new(
            paths.log_path.clone(),
            crate::run::RETAIN_DAILY_LOG_FILES,
        )?;
        let json_writer = crate::run::RotatingLogWriter::new(
            crate::run::json_log_path(&paths.log_path),
            crate::run::RETAIN_DAILY_LOG_FILES,
        )?;

        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_writer(move || writer.clone()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_span_list(false)
                    .with_current_span(true)
                    .with_writer(move || json_writer.clone()),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_span_list(false)
                    .with_current_span(true)
                    .with_writer(std::io::stderr),
            )
            .init();
    }
    Ok(())
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
                "warning: failed to read {} for execution.log_level: {err}",
                paths.config_path.display()
            );
            return LogLevel::default();
        }
    };

    match serde_yaml::from_str::<GlobalConfig>(&content) {
        Ok(global) => global.execution.log_level,
        Err(err) => {
            eprintln!(
                "warning: failed to parse {} for execution.log_level: {err}",
                paths.config_path.display()
            );
            LogLevel::default()
        }
    }
}
