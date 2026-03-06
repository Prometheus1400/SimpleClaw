use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use color_eyre::eyre::WrapErr;
use tracing::info;

use crate::agent::AgentRuntime;
use crate::channel::{Channel, DiscordChannel, LoggingChannel};
use crate::cli::Cli;
use crate::config::{ChannelKind, LoadedConfig, ProviderKind};
use crate::gateway::Gateway;
use crate::memory::MemoryStore;
use crate::paths::AppPaths;
use crate::prompt::PromptAssembler;
use crate::provider::GeminiProvider;

pub async fn run_service(cli: &Cli) -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    paths
        .ensure_db_dir()
        .wrap_err("failed to create database directory")?;

    let loaded = LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load global/workspace configuration")?;

    let workspace_exists = loaded.workspace.is_dir();
    info!(
        workspace_raw = %loaded.workspace_raw.display(),
        workspace_resolved = %loaded.workspace.display(),
        workspace_exists,
        "workspace path resolved"
    );

    let prompt_layers = PromptAssembler::inspect_workspace(&loaded.workspace)
        .wrap_err("failed to inspect workspace prompt layers")?;
    for layer in &prompt_layers {
        info!(
            layer = layer.title,
            file = layer.file,
            path = %layer.path.display(),
            exists = layer.exists,
            bytes = layer.bytes,
            "prompt layer status"
        );
    }

    let system_prompt = PromptAssembler::from_workspace(&loaded.workspace)
        .wrap_err("failed to assemble layered system prompt")?;
    info!(
        prompt_chars = system_prompt.chars().count(),
        "layered system prompt assembled"
    );

    let memory = MemoryStore::new(
        &paths.db_path,
        &loaded.global.database,
        &loaded.global.embedding,
    )
    .await
    .wrap_err("failed to initialize sqlite memory store")?;

    let channel: Arc<dyn Channel> = match loaded.agent.routing.channel {
        ChannelKind::Discord => Arc::new(
            DiscordChannel::from_config(&loaded.global.discord)
                .await
                .wrap_err("failed to initialize discord channel")?,
        ),
        ChannelKind::Logging => Arc::new(LoggingChannel::default()),
    };
    let provider: Arc<dyn crate::provider::Provider> = match loaded.global.provider.kind {
        ProviderKind::Gemini => {
            Arc::new(GeminiProvider::from_config(loaded.global.provider.clone()))
        }
    };
    let workspace_display = loaded.workspace.display().to_string();

    let runtime = AgentRuntime::new(loaded, provider, memory, system_prompt, cli.max_steps);
    let gateway = Gateway::new(Arc::clone(&channel));

    info!(workspace = %workspace_display, "runtime initialized");
    loop {
        let inbound = match gateway.next_message().await {
            Ok(msg) => msg,
            Err(err) => {
                tracing::error!(error = %err, "gateway listen failed");
                continue;
            }
        };

        if !inbound.invoke {
            tracing::debug!(
                session_id = %inbound.session_id,
                channel_id = %inbound.channel_id,
                guild_id = inbound.guild_id.as_deref().unwrap_or("dm"),
                user_id = %inbound.user_id,
                "recording passive channel context message"
            );
            if let Err(err) = runtime.record_context(&inbound).await {
                tracing::error!(error = %err, "failed to persist passive context message");
            }
            continue;
        }

        if let Err(err) = channel.broadcast_typing(&inbound.session_id).await {
            tracing::warn!(error = %err, "failed to broadcast typing");
        }
        tracing::debug!(
            session_id = %inbound.session_id,
            channel_id = %inbound.channel_id,
            guild_id = inbound.guild_id.as_deref().unwrap_or("dm"),
            is_dm = inbound.is_dm,
            user_id = %inbound.user_id,
            mentioned_bot = inbound.mentioned_bot,
            "dispatching inbound message to agent"
        );

        match runtime.run(&inbound).await {
            Ok(reply) => {
                if let Err(err) = channel.send_message(&inbound.session_id, &reply).await {
                    tracing::error!(error = %err, "failed to send channel response");
                }
            }
            Err(err) => {
                tracing::error!(error = %err, "agent execution failed");
                if let Err(send_err) = channel
                    .send_message(
                        &inbound.session_id,
                        &runtime.config().global.runtime.safe_error_reply,
                    )
                    .await
                {
                    tracing::error!(error = %send_err, "failed to send safe error reply");
                }
            }
        }
    }
}

pub fn start_service(cli: &Cli) -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    paths
        .ensure_runtime_dirs()
        .wrap_err("failed to create runtime state directories")?;
    let pid_path = paths.pid_path;
    let log_path = paths.log_path;

    if let Some(pid) = read_pid(&pid_path)? {
        if is_process_running(pid) {
            println!("service already running (pid {pid})");
            return Ok(());
        }
        fs::remove_file(&pid_path).wrap_err("failed to remove stale pid file")?;
    }

    let exe = std::env::current_exe().wrap_err("failed to resolve current executable path")?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .wrap_err("failed to open service log file")?;
    let stderr = stdout
        .try_clone()
        .wrap_err("failed to create stderr log handle")?;

    let mut child = ProcessCommand::new(exe);
    child
        .arg("--max-steps")
        .arg(cli.max_steps.to_string())
        .arg("system")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(workspace) = &cli.workspace {
        child.arg("--workspace").arg(workspace);
    }

    let child = child
        .spawn()
        .wrap_err("failed to launch background service")?;
    let pid = child.id();
    fs::write(&pid_path, format!("{pid}\n")).wrap_err("failed to write pid file")?;

    println!("service started (pid {pid})");
    println!("pid file: {}", pid_path.display());
    println!("log file: {}", log_path.display());
    Ok(())
}

pub fn stop_service(cli: &Cli) -> color_eyre::Result<()> {
    let pid_path = state_paths(cli)?.pid_path;
    let Some(pid) = read_pid(&pid_path)? else {
        println!("service is not running");
        return Ok(());
    };

    if !is_process_running(pid) {
        let _ = fs::remove_file(&pid_path);
        println!("service is not running (removed stale pid file)");
        return Ok(());
    }

    terminate_process(pid).wrap_err("failed to send stop signal")?;
    if wait_for_exit(pid, Duration::from_secs(5)) {
        let _ = fs::remove_file(&pid_path);
        println!("service stopped");
        return Ok(());
    }

    force_kill_process(pid).wrap_err("failed to force stop service")?;
    if wait_for_exit(pid, Duration::from_secs(2)) {
        let _ = fs::remove_file(&pid_path);
        println!("service stopped (forced)");
        return Ok(());
    }

    Err(color_eyre::eyre::eyre!(
        "service did not stop after termination attempts"
    ))
}

pub fn show_logs(cli: &Cli, follow: bool) -> color_eyre::Result<()> {
    let log_path = state_paths(cli)?.log_path;
    if !log_path.exists() {
        println!("no logs found at {}", log_path.display());
        return Ok(());
    }

    if !follow {
        let content = fs::read_to_string(&log_path).wrap_err("failed to read log file")?;
        print!("{content}");
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .read(true)
        .open(&log_path)
        .wrap_err("failed to open log file for follow mode")?;

    let mut cursor = 0_u64;
    loop {
        let file_len = file.metadata()?.len();
        if file_len < cursor {
            cursor = 0;
        }
        if file_len > cursor {
            file.seek(SeekFrom::Start(cursor))?;
            let mut chunk = Vec::new();
            file.read_to_end(&mut chunk)?;
            std::io::stdout().write_all(&chunk)?;
            std::io::stdout().flush()?;
            cursor = file.stream_position()?;
        }
        thread::sleep(Duration::from_millis(300));
    }
}

pub fn show_status(cli: &Cli) -> color_eyre::Result<()> {
    let paths = state_paths(cli)?;
    let state_dir = paths.run_dir;
    let pid_path = paths.pid_path;
    let log_path = paths.log_path;
    let pid = read_pid(&pid_path)?;

    match pid {
        Some(pid_value) if is_process_running(pid_value) => {
            println!("status: running");
            println!("pid: {pid_value}");
        }
        Some(pid_value) => {
            println!("status: stopped (stale pid file)");
            println!("pid: {pid_value}");
        }
        None => println!("status: stopped"),
    }

    println!("state dir: {}", state_dir.display());
    println!("pid file: {}", pid_path.display());
    println!("log file: {}", log_path.display());
    if let Ok(metadata) = fs::metadata(&log_path) {
        println!("log size: {} bytes", metadata.len());
    }
    Ok(())
}

fn state_paths(_cli: &Cli) -> color_eyre::Result<AppPaths> {
    AppPaths::resolve().map_err(Into::into)
}

fn read_pid(path: &Path) -> color_eyre::Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).wrap_err("failed to read pid file")?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .wrap_err("failed to parse pid file content")?;
    Ok(Some(pid))
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let step = Duration::from_millis(100);
    let polls = timeout.as_millis() / step.as_millis();
    for _ in 0..polls {
        if !is_process_running(pid) {
            return true;
        }
        thread::sleep(step);
    }
    !is_process_running(pid)
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    ProcessCommand::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains(&pid.to_string()))
        })
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .status()?;
    Ok(())
}

#[cfg(unix)]
fn force_kill_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn force_kill_process(pid: u32) -> std::io::Result<()> {
    ProcessCommand::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()?;
    Ok(())
}
