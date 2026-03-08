use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use tracing::{info, info_span};

use crate::channels::InboundMessage;
use crate::cli::{Cli, MemoryMode};
use crate::config::{AgentEntryConfig, LoadedConfig};
use crate::paths::AppPaths;

pub(crate) mod composition;
mod daemon;
mod logging;
mod session;

pub use daemon::{start_service, stop_service};
pub(crate) use logging::json_log_path;
pub use logging::{RETAIN_DAILY_LOG_FILES, RotatingLogWriter};

use composition::{
    RuntimeDependencies, RuntimeState, agent_workspace_memory_paths, assemble_runtime_state,
};
use daemon::{is_process_running, read_pid, state_paths};
use logging::collect_log_history;
use session::{SessionHandler, SessionWorkerCoordinator};

const SESSION_WORKER_IDLE_TIMEOUT_SECS: u64 = 300;

#[tracing::instrument(
    name = "inbound.message",
    skip(state, inbound),
    fields(
        trace_id = %inbound.trace_id,
        session_id = %inbound.session_key,
        agent_id = %inbound.target_agent_id
    )
)]
pub(crate) async fn handle_inbound_once(
    state: &RuntimeState,
    inbound: InboundMessage,
) -> color_eyre::Result<()> {
    let memory_session_id = inbound.session_key.clone();
    let started = std::time::Instant::now();
    info!(status = "started", "inbound lifecycle");
    let Some(runtime) = state.runtimes.get(&inbound.target_agent_id) else {
        tracing::error!(
            status = "dropped",
            error_kind = "unknown_agent",
            channel_id = %inbound.channel_id,
            "inbound message dropped"
        );
        if inbound.invoke
            && let Err(err) = state
                .gateway
                .send_message(
                    &inbound,
                    "I couldn't route that message due to a configuration error.",
                )
                .await
        {
            tracing::error!(
                status = "failed",
                error_kind = "channel_send",
                error = %err,
                "route reply failed"
            );
        }
        return Ok(());
    };

    if !inbound.invoke {
        tracing::debug!(status = "recording_context", "passive inbound");
        if let Err(err) = runtime
            .record_context(&inbound, &memory_session_id, state.context.agents.as_ref())
            .await
        {
            tracing::error!(
                status = "failed",
                error_kind = "memory_write",
                error = %err,
                "passive context persist failed"
            );
        }
        info!(
            status = "completed",
            elapsed_ms = started.elapsed().as_millis() as u64,
            "inbound lifecycle"
        );
        return Ok(());
    }

    if inbound.user_id != "system"
        && let Err(err) = state.gateway.broadcast_typing(&inbound).await
    {
        tracing::warn!(
            status = "failed",
            error_kind = "typing_broadcast",
            error = %err,
            "typing broadcast failed"
        );
    }
    tracing::debug!(status = "dispatching", "invoke inbound");

    match runtime
        .run(&inbound, &memory_session_id, state.context.as_ref())
        .await
    {
        Ok(reply) => {
            if let Err(err) = state.gateway.send_message(&inbound, &reply).await {
                tracing::error!(
                    status = "failed",
                    error_kind = "channel_send",
                    error = %err,
                    "channel response failed"
                );
            }
        }
        Err(err) => {
            tracing::error!(
                status = "failed",
                error_kind = "agent_runtime",
                error = %err,
                "agent execution failed"
            );
            if let Err(send_err) = state
                .gateway
                .send_message(&inbound, &state.context.safe_error_reply)
                .await
            {
                tracing::error!(
                    status = "failed",
                    error_kind = "safe_reply_send",
                    error = %send_err,
                    "safe error reply send failed"
                );
            }
        }
    }
    info!(
        status = "completed",
        elapsed_ms = started.elapsed().as_millis() as u64,
        "inbound lifecycle"
    );
    Ok(())
}

pub async fn run_service(cli: &Cli) -> color_eyre::Result<()> {
    let app_paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    let loaded = LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load global/workspace configuration")?;
    let deps = RuntimeDependencies::default();
    let (state, mut inbound_rx) = assemble_runtime_state(cli, &loaded, &app_paths, &deps).await?;
    let state = Arc::new(state);
    let coordinator =
        SessionWorkerCoordinator::new(Duration::from_secs(SESSION_WORKER_IDLE_TIMEOUT_SECS));
    let handler: SessionHandler<InboundMessage> = {
        let state = Arc::clone(&state);
        Arc::new(move |inbound: InboundMessage| {
            let state = Arc::clone(&state);
            Box::pin(async move {
                if let Err(err) = handle_inbound_once(state.as_ref(), inbound).await {
                    tracing::error!(error = %err, "failed to process inbound message");
                }
            })
        })
    };

    let service_span = info_span!("service.run");
    let _service_entered = service_span.enter();
    info!(status = "started", "service runtime initialized");
    loop {
        let Some(inbound) = inbound_rx.recv().await else {
            tracing::error!(
                status = "failed",
                error_kind = "gateway_listen",
                "gateway inbound channel closed"
            );
            continue;
        };
        let key = inbound.session_key.clone();
        coordinator
            .dispatch(key, inbound, Arc::clone(&handler))
            .await;
    }
}

pub fn show_logs(cli: &Cli, follow: bool) -> color_eyre::Result<()> {
    let log_path = state_paths(cli)?.log_path;
    let history = collect_log_history(&log_path).wrap_err("failed to list log history")?;
    if history.is_empty() {
        println!("no logs found at {}", log_path.display());
        return Ok(());
    }

    if !follow {
        for path in history {
            let content = fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            print!("{content}");
        }
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
    let json_log_path = json_log_path(&log_path);
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
    println!("json log file: {}", json_log_path.display());
    if let Ok(metadata) = fs::metadata(&log_path) {
        println!("log size: {} bytes", metadata.len());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct AgentMemoryResponse {
    agent: String,
    memory: String,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    short: Option<Vec<ShortMemoryRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long: Option<Vec<LongMemoryRow>>,
}

#[derive(Debug, Serialize)]
struct ShortMemoryRow {
    id: i64,
    session_id: String,
    role: String,
    content: String,
    username: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct LongMemoryRow {
    id: i64,
    source_session_id: String,
    content: String,
    kind: String,
    importance: i64,
    created_at: String,
}

pub async fn show_agent_memory(
    cli: &Cli,
    agent_id: &str,
    memory: MemoryMode,
    limit: usize,
) -> color_eyre::Result<()> {
    let loaded = LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load configuration for agent memory command")?;
    let agent = resolve_agent(&loaded.global.agents.list, agent_id)?;
    let (_memory_dir, short_term_path, long_term_path) =
        agent_workspace_memory_paths(&agent.workspace);

    let short = if matches!(memory, MemoryMode::Short | MemoryMode::Both) {
        Some(query_short_memory(&short_term_path, limit)?)
    } else {
        None
    };
    let long = if matches!(memory, MemoryMode::Long | MemoryMode::Both) {
        Some(query_long_memory(&long_term_path, limit)?)
    } else {
        None
    };

    let response = AgentMemoryResponse {
        agent: agent_id.to_owned(),
        memory: memory_mode_name(memory).to_owned(),
        limit,
        short,
        long,
    };
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn memory_mode_name(memory: MemoryMode) -> &'static str {
    match memory {
        MemoryMode::Short => "short",
        MemoryMode::Long => "long",
        MemoryMode::Both => "both",
    }
}

fn resolve_agent<'a>(
    agents: &'a [AgentEntryConfig],
    agent_id: &str,
) -> color_eyre::Result<&'a AgentEntryConfig> {
    agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| {
            let available = agents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            color_eyre::eyre::eyre!("unknown agent id '{agent_id}'. configured agents: {available}")
        })
}

fn open_readonly_connection(path: &Path) -> color_eyre::Result<Connection> {
    if !path.exists() {
        return Err(color_eyre::eyre::eyre!(
            "memory database does not exist: {}",
            path.display()
        ));
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open database read-only: {}", path.display()))
}

fn query_short_memory(path: &Path, limit: usize) -> color_eyre::Result<Vec<ShortMemoryRow>> {
    let conn = open_readonly_connection(path)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, role, content, username, created_at
             FROM messages
             ORDER BY id DESC
             LIMIT ?1",
        )
        .wrap_err_with(|| format!("failed to prepare short-term query for {}", path.display()))?;

    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok::<ShortMemoryRow, rusqlite::Error>(ShortMemoryRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                username: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .wrap_err_with(|| format!("failed to query messages table in {}", path.display()))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn query_long_memory(path: &Path, limit: usize) -> color_eyre::Result<Vec<LongMemoryRow>> {
    let conn = open_readonly_connection(path)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, source_session_id, content, kind, importance, created_at
             FROM ltm_facts
             ORDER BY id DESC
             LIMIT ?1",
        )
        .wrap_err_with(|| format!("failed to prepare long-term query for {}", path.display()))?;

    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok::<LongMemoryRow, rusqlite::Error>(LongMemoryRow {
                id: row.get(0)?,
                source_session_id: row.get(1)?,
                content: row.get(2)?,
                kind: row.get(3)?,
                importance: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .wrap_err_with(|| format!("failed to query ltm_facts table in {}", path.display()))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use super::{query_long_memory, query_short_memory};

    fn temp_db_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic enough for tests")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}.db"))
    }

    fn with_temp_db<F>(path: &Path, setup: F)
    where
        F: FnOnce(&Connection),
    {
        let conn = Connection::open(path).expect("should create temporary sqlite db");
        setup(&conn);
        drop(conn);
    }

    #[test]
    fn short_memory_returns_newest_first_with_limit() {
        let path = temp_db_path("short");
        with_temp_db(&path, |conn| {
            conn.execute_batch(
                r#"
                CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    username TEXT,
                    created_at TEXT NOT NULL
                );
                "#,
            )
            .expect("messages table should be created");
            for i in 0..3 {
                conn.execute(
                    "INSERT INTO messages (session_id, role, content, username, created_at) VALUES (?1, 'user', ?2, 'kaleb', ?3)",
                    rusqlite::params!["chan:agent", format!("msg-{i}"), format!("2026-03-06T00:00:0{i}Z")],
                )
                .expect("message insert should succeed");
            }
        });

        let rows = query_short_memory(&path, 2).expect("short query should succeed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].content, "msg-2");
        assert_eq!(rows[1].content, "msg-1");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn long_memory_returns_newest_first_with_limit() {
        let path = temp_db_path("long");
        with_temp_db(&path, |conn| {
            conn.execute_batch(
                r#"
                CREATE TABLE ltm_facts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    source_session_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    importance INTEGER NOT NULL,
                    embedding BLOB,
                    created_at TEXT NOT NULL
                );
                "#,
            )
            .expect("ltm_facts table should be created");
            for i in 0..3 {
                conn.execute(
                    "INSERT INTO ltm_facts (source_session_id, content, kind, importance, embedding, created_at) VALUES (?1, ?2, 'general', 3, NULL, ?3)",
                    rusqlite::params!["chan:agent", format!("fact-{i}"), format!("2026-03-06T00:00:0{i}Z")],
                )
                .expect("ltm_facts insert should succeed");
            }
        });

        let rows = query_long_memory(&path, 2).expect("long query should succeed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].content, "fact-2");
        assert_eq!(rows[1].content, "fact-1");

        let _ = fs::remove_file(&path);
    }
}
