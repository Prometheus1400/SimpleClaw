use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use tokio::sync::Notify;
use tracing::{info, info_span};

use crate::channels::InboundMessage;
use crate::cli::{Cli, MemoryMode};
use crate::config::{AgentEntryConfig, ChannelOutputMode, LoadedConfig};
use crate::paths::AppPaths;
use crate::reply_policy::is_no_reply;

pub(crate) mod composition;
mod cron_scheduler;
mod daemon;
mod logging;
mod session;
mod transparency;

pub use daemon::{start_service, stop_service};
pub(crate) use logging::json_log_path;
pub use logging::{RETAIN_DAILY_LOG_FILES, RotatingLogWriter};

use composition::{
    RuntimeDependencies, RuntimeState, agent_persona_memory_paths, assemble_runtime_state,
    start_runtime_services,
};
use daemon::{is_process_running, read_pid, state_paths};
use logging::collect_log_history;
use session::{SessionHandler, SessionWorkerCoordinator};

const SESSION_WORKER_IDLE_TIMEOUT_SECS: u64 = 300;
const INBOUND_ACK_REACTION: &str = "👀";
const STREAMING_EDIT_INTERVAL: Duration = Duration::from_millis(1_500);

struct StreamingDisplay {
    gateway: Arc<crate::gateway::Gateway>,
    inbound: InboundMessage,
    channel_limit: Option<usize>,
    latest_content: String,
    committed_prefix_chars: usize,
    displayed_segment: Option<String>,
    message_id: Option<String>,
    last_edit: Instant,
    initial_send_attempted: bool,
    send_in_flight: bool,
    edit_in_flight: bool,
    finalized: bool,
    terminal_failure: bool,
    error_message: Option<String>,
    notify: Arc<Notify>,
}

impl StreamingDisplay {
    fn new(
        gateway: Arc<crate::gateway::Gateway>,
        inbound: InboundMessage,
        channel_limit: Option<usize>,
    ) -> Self {
        Self {
            gateway,
            inbound,
            channel_limit,
            latest_content: String::new(),
            committed_prefix_chars: 0,
            displayed_segment: None,
            message_id: None,
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            initial_send_attempted: false,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
        }
    }
}

struct ActiveStreamingSegment {
    content: String,
    visible_chars: usize,
    has_overflow: bool,
}

fn byte_index_for_char_offset(content: &str, char_offset: usize) -> usize {
    if char_offset == 0 {
        return 0;
    }
    content
        .char_indices()
        .nth(char_offset)
        .map(|(idx, _)| idx)
        .unwrap_or(content.len())
}

fn active_streaming_segment(
    latest_content: &str,
    committed_prefix_chars: usize,
    channel_limit: Option<usize>,
) -> Option<ActiveStreamingSegment> {
    let start = byte_index_for_char_offset(latest_content, committed_prefix_chars);
    let tail = &latest_content[start..];
    if tail.is_empty() {
        return None;
    }

    let tail_chars = tail.chars().count();
    let visible_chars = channel_limit.map_or(tail_chars, |limit| limit.min(tail_chars));
    let end = byte_index_for_char_offset(tail, visible_chars);
    Some(ActiveStreamingSegment {
        content: tail[..end].to_owned(),
        visible_chars,
        has_overflow: tail_chars > visible_chars,
    })
}

fn try_rollover_streaming_segment(state: &mut StreamingDisplay) -> bool {
    if state.send_in_flight || state.edit_in_flight || state.terminal_failure {
        return false;
    }
    let Some(segment) = active_streaming_segment(
        &state.latest_content,
        state.committed_prefix_chars,
        state.channel_limit,
    ) else {
        return false;
    };
    if !segment.has_overflow || state.displayed_segment.as_deref() != Some(segment.content.as_str())
    {
        return false;
    }

    state.committed_prefix_chars += segment.visible_chars;
    state.displayed_segment = None;
    state.message_id = None;
    state.initial_send_attempted = false;
    state.error_message = None;
    true
}

enum StreamingDisplayAction {
    SendInitial {
        gateway: Arc<crate::gateway::Gateway>,
        inbound: InboundMessage,
        content: String,
    },
    Edit {
        gateway: Arc<crate::gateway::Gateway>,
        inbound: InboundMessage,
        message_id: String,
        content: String,
    },
}

fn spawn_streaming_display_update(display: &Arc<Mutex<StreamingDisplay>>, content: &str) {
    {
        let mut state = match display.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.latest_content = content.to_owned();
        state.notify.notify_waiters();
    }
    spawn_next_streaming_display_action(display);
}

fn next_streaming_display_action(
    display: &Arc<Mutex<StreamingDisplay>>,
) -> Option<StreamingDisplayAction> {
    let mut state = match display.lock() {
        Ok(state) => state,
        Err(_) => return None,
    };

    if state.terminal_failure || state.latest_content.is_empty() {
        return None;
    }

    if try_rollover_streaming_segment(&mut state) {
        state.notify.notify_waiters();
    }

    let segment = active_streaming_segment(
        &state.latest_content,
        state.committed_prefix_chars,
        state.channel_limit,
    )?;

    if state.message_id.is_none() {
        if state.send_in_flight || state.displayed_segment.is_some() || state.initial_send_attempted
        {
            return None;
        }
        state.initial_send_attempted = true;
        state.send_in_flight = true;
        return Some(StreamingDisplayAction::SendInitial {
            gateway: Arc::clone(&state.gateway),
            inbound: state.inbound.clone(),
            content: segment.content,
        });
    }

    if state.edit_in_flight {
        return None;
    }

    if state.displayed_segment.as_deref() == Some(segment.content.as_str()) {
        return None;
    }

    if !state.finalized && state.last_edit.elapsed() < STREAMING_EDIT_INTERVAL {
        return None;
    }

    let message_id = state.message_id.clone()?;
    state.edit_in_flight = true;
    Some(StreamingDisplayAction::Edit {
        gateway: Arc::clone(&state.gateway),
        inbound: state.inbound.clone(),
        message_id,
        content: segment.content,
    })
}

fn spawn_next_streaming_display_action(display: &Arc<Mutex<StreamingDisplay>>) {
    let Some(action) = next_streaming_display_action(display) else {
        return;
    };
    let display = Arc::clone(display);
    tokio::spawn(async move {
        match action {
            StreamingDisplayAction::SendInitial {
                gateway,
                inbound,
                content,
            } => {
                let result = gateway.send_message_with_id(&inbound, &content).await;
                let mut should_retry = false;
                {
                    let mut state = match display.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    state.send_in_flight = false;
                    match result {
                        Ok(Some(message_id)) => {
                            state.message_id = Some(message_id);
                            state.displayed_segment = Some(content);
                            state.last_edit = Instant::now();
                            state.error_message = None;
                            should_retry = true;
                        }
                        Ok(None) => {
                            state.displayed_segment = Some(content);
                            state.error_message = None;
                        }
                        Err(err) => {
                            state.error_message = Some(err.to_string());
                            state.terminal_failure = true;
                            tracing::warn!(
                                status = "failed",
                                error_kind = "streaming_initial_send",
                                error = %err,
                                trace_id = %inbound.trace_id,
                                session_id = %inbound.session_key,
                                "streaming initial send failed"
                            );
                        }
                    }
                    state.notify.notify_waiters();
                }
                if should_retry {
                    spawn_next_streaming_display_action(&display);
                }
            }
            StreamingDisplayAction::Edit {
                gateway,
                inbound,
                message_id,
                content,
            } => {
                let result = gateway.edit_message(&inbound, &message_id, &content).await;
                let mut should_retry = false;
                {
                    let mut state = match display.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    state.edit_in_flight = false;
                    if let Err(err) = result {
                        state.error_message = Some(err.to_string());
                        state.terminal_failure = true;
                        tracing::warn!(
                            status = "failed",
                            error_kind = "streaming_edit",
                            error = %err,
                            trace_id = %inbound.trace_id,
                            session_id = %inbound.session_key,
                            message_id = %message_id,
                            "streaming edit failed"
                        );
                    } else {
                        state.displayed_segment = Some(content);
                        state.last_edit = Instant::now();
                        state.error_message = None;
                        should_retry = true;
                    }
                    state.notify.notify_waiters();
                }
                if should_retry {
                    spawn_next_streaming_display_action(&display);
                }
            }
        }
    });
}

async fn finalize_streaming_display(
    display: &Arc<Mutex<StreamingDisplay>>,
    content: &str,
) -> Result<(), crate::error::FrameworkError> {
    {
        let mut state = display.lock().map_err(|_| {
            crate::error::FrameworkError::Tool("streaming display mutex poisoned".to_owned())
        })?;
        state.latest_content = content.to_owned();
        state.finalized = true;
        state.notify.notify_waiters();
    }
    spawn_next_streaming_display_action(display);

    loop {
        let fallback = {
            let state = display.lock().map_err(|_| {
                crate::error::FrameworkError::Tool("streaming display mutex poisoned".to_owned())
            })?;
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.terminal_failure
                && let Some(error_message) = state.error_message.clone()
            {
                if state.message_id.is_some()
                    || state.displayed_segment.is_some()
                    || state.committed_prefix_chars > 0
                {
                    return Err(crate::error::FrameworkError::Tool(error_message));
                }
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.committed_prefix_chars == content.chars().count()
            {
                return Ok(());
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && active_streaming_segment(
                    &state.latest_content,
                    state.committed_prefix_chars,
                    state.channel_limit,
                )
                .map(|segment| state.displayed_segment.as_deref() == Some(segment.content.as_str()))
                .unwrap_or(false)
                && state.committed_prefix_chars
                    + active_streaming_segment(
                        &state.latest_content,
                        state.committed_prefix_chars,
                        state.channel_limit,
                    )
                    .map(|segment| segment.visible_chars)
                    .unwrap_or(0)
                    == content.chars().count()
            {
                return Ok(());
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.message_id.is_none()
                && state.displayed_segment.is_none()
            {
                Some((Arc::clone(&state.gateway), state.inbound.clone()))
            } else {
                None
            }
        };

        if let Some((gateway, inbound)) = fallback {
            return gateway.send_message(&inbound, content).await;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
        spawn_next_streaming_display_action(display);
    }
}

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

    let streaming_display = (state.gateway.output_mode(&inbound) == ChannelOutputMode::Streaming
        && state
            .gateway
            .supports_message_editing(&inbound)
            .unwrap_or(false))
    .then(|| {
        let channel_limit = state.gateway.message_char_limit(&inbound).unwrap_or(None);
        Arc::new(Mutex::new(StreamingDisplay::new(
            Arc::clone(&state.gateway),
            inbound.clone(),
            channel_limit,
        )))
    });
    let on_text_delta = streaming_display.as_ref().map(|streaming_display| {
        let streaming_display = Arc::clone(streaming_display);
        Arc::new(move |text: &str| {
            spawn_streaming_display_update(&streaming_display, text);
        }) as Arc<dyn Fn(&str) + Send + Sync>
    });

    match runtime
        .run(
            &inbound,
            &memory_session_id,
            state.context.as_ref(),
            on_text_delta,
        )
        .await
    {
        Ok(outcome) => {
            if is_no_reply(&outcome.reply) {
                tracing::debug!(
                    status = "suppressed",
                    reason = "no_reply_sentinel",
                    "outbound reply"
                );
                info!(
                    status = "completed",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "inbound lifecycle"
                );
                return Ok(());
            }
            let transparency = runtime.transparency();
            let outbound = transparency::render_tool_call_transparency(
                &outcome.reply,
                &outcome.tool_calls,
                transparency.tool_calls,
                transparency.memory_recall,
                outcome.memory_recall_used,
                outcome.memory_recall_short_hits,
                outcome.memory_recall_long_hits,
                inbound.source_channel,
            );
            let send_result = if let Some(streaming_display) = streaming_display.as_ref() {
                finalize_streaming_display(streaming_display, &outbound).await
            } else {
                state.gateway.send_message(&inbound, &outbound).await
            };
            if let Err(err) = send_result {
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
            let send_result = if let Some(streaming_display) = streaming_display.as_ref() {
                finalize_streaming_display(streaming_display, &state.safe_error_reply).await
            } else {
                state
                    .gateway
                    .send_message(&inbound, &state.safe_error_reply)
                    .await
            };
            if let Err(send_err) = send_result {
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

pub async fn run_service() -> color_eyre::Result<()> {
    let app_paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    let loaded = LoadedConfig::load(None).wrap_err("failed to load global configuration")?;
    let deps = RuntimeDependencies::default();
    let (state, mut inbound_rx) = assemble_runtime_state(&loaded, &app_paths, &deps).await?;
    let state = Arc::new(state);
    let _runtime_services = start_runtime_services(state.as_ref());
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
        dispatch_inbound_with_ack(
            &coordinator,
            Arc::clone(&handler),
            state.gateway.as_ref(),
            inbound,
        )
        .await;
    }
}

async fn dispatch_inbound_with_ack(
    coordinator: &SessionWorkerCoordinator<InboundMessage>,
    handler: SessionHandler<InboundMessage>,
    gateway: &crate::gateway::Gateway,
    inbound: InboundMessage,
) {
    let key = inbound.session_key.clone();
    let queued = coordinator.dispatch(key, inbound.clone(), handler).await;
    if !queued || !inbound.invoke || inbound.user_id == "system" {
        return;
    }
    let Some(message_id) = inbound.source_message_id.as_deref() else {
        return;
    };

    if let Err(err) = gateway
        .add_reaction(
            inbound.source_channel,
            &inbound.channel_id,
            message_id,
            INBOUND_ACK_REACTION,
        )
        .await
    {
        tracing::warn!(
            status = "failed",
            error_kind = "inbound_ack",
            error = %err,
            trace_id = %inbound.trace_id,
            session_id = %inbound.session_key,
            channel_id = %inbound.channel_id,
            message_id = %message_id,
            "inbound ack reaction failed"
        );
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
        agent_persona_memory_paths(&agent.persona);

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
    use std::collections::HashMap;
    use std::fs;
    use std::future::pending;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use rusqlite::Connection;
    use tokio::sync::{Mutex, Notify, mpsc};
    use tokio::time::{Duration, timeout};

    use super::{
        INBOUND_ACK_REACTION, STREAMING_EDIT_INTERVAL, StreamingDisplay, dispatch_inbound_with_ack,
        finalize_streaming_display, handle_inbound_once, query_long_memory, query_short_memory,
        spawn_streaming_display_update,
    };
    use crate::agent::{
        AgentDirectory, AgentRuntime, AgentRuntimeConfig, RuntimeContext, ToolRuntime,
    };
    use crate::channels::{Channel, ChannelInbound, InboundMessage};
    use crate::config::{
        AgentInnerConfig, ChannelOutputMode, ExecutionDefaultsConfig, GatewayChannelKind,
        MemoryRecallConfig, RoutingConfig,
    };
    use crate::error::FrameworkError;
    use crate::gateway::Gateway;
    use crate::memory::{
        DynMemory, LongTermFactSummary, LongTermForgetResult, MemorizeResult, Memory,
        MemoryRecallHit, MemoryStoreScope, StoredMessage, StoredRole,
    };
    use crate::providers::{
        Message, Provider, ProviderFactory, ProviderResponse, ProviderStream, StreamEvent,
        ToolDefinition,
    };
    use crate::react::ReactLoop;
    use crate::run::session::{SessionHandler, SessionWorkerCoordinator};
    use crate::telemetry::next_trace_id;
    use crate::tools::skill::SkillFactory;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, InvokeOutcome, ProcessManager, default_factory,
    };

    use super::composition::RuntimeState;

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

    #[derive(Default)]
    struct AckCaptureChannel {
        reactions: Mutex<Vec<(String, String, String)>>,
        fail_reaction: AtomicBool,
    }

    impl AckCaptureChannel {
        fn with_reaction_failure() -> Self {
            Self {
                reactions: Mutex::new(Vec::new()),
                fail_reaction: AtomicBool::new(true),
            }
        }

        async fn reactions(&self) -> Vec<(String, String, String)> {
            self.reactions.lock().await.clone()
        }
    }

    #[derive(Default)]
    struct FakeMemory {
        appended: Mutex<Vec<(String, StoredRole, String, Option<String>)>>,
    }

    impl FakeMemory {
        async fn appended(&self) -> Vec<(String, StoredRole, String, Option<String>)> {
            self.appended.lock().await.clone()
        }
    }

    #[async_trait]
    impl Memory for FakeMemory {
        async fn append_message(
            &self,
            session_id: &str,
            role: StoredRole,
            content: &str,
            username: Option<&str>,
        ) -> Result<(), FrameworkError> {
            self.appended.lock().await.push((
                session_id.to_owned(),
                role,
                content.to_owned(),
                username.map(str::to_owned),
            ));
            Ok(())
        }

        async fn semantic_query_combined(
            &self,
            _session_id: &str,
            _query: &str,
            _top_k: usize,
            _history_window: usize,
            _scope: MemoryStoreScope,
        ) -> Result<Vec<String>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn query_recall_hits(
            &self,
            _session_id: &str,
            _query: &str,
            _config: &MemoryRecallConfig,
            _history_window: usize,
            _scope: MemoryStoreScope,
            _prefer_long_term: bool,
        ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn semantic_forget_long_term(
            &self,
            _query: &str,
            _similarity_threshold: f32,
            _max_matches: usize,
            _kind_filter: Option<&str>,
            _commit: bool,
        ) -> Result<LongTermForgetResult, FrameworkError> {
            Ok(LongTermForgetResult {
                matches: Vec::new(),
                deleted_count: 0,
                similarity_threshold: 0.0,
                max_matches: 0,
                kind_filter: None,
            })
        }

        async fn recent_messages(
            &self,
            _session_id: &str,
            _limit: usize,
        ) -> Result<Vec<StoredMessage>, FrameworkError> {
            Ok(Vec::new())
        }

        async fn memorize(
            &self,
            _session_id: &str,
            _content: &str,
            _kind: &str,
            _importance: u8,
        ) -> Result<MemorizeResult, FrameworkError> {
            Ok(MemorizeResult::Inserted)
        }

        async fn list_long_term_facts(
            &self,
            _kind_filter: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<LongTermFactSummary>, FrameworkError> {
            Ok(Vec::new())
        }
    }

    struct StaticProvider {
        reply: Option<String>,
        error: Option<String>,
    }

    impl StaticProvider {
        fn ok(reply: impl Into<String>) -> Self {
            Self {
                reply: Some(reply.into()),
                error: None,
            }
        }

        fn err(message: impl Into<String>) -> Self {
            Self {
                reply: None,
                error: Some(message.into()),
            }
        }
    }

    #[async_trait]
    impl Provider for StaticProvider {
        async fn generate(
            &self,
            _system_prompt: &str,
            _history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            if let Some(err) = &self.error {
                return Err(FrameworkError::Provider(err.clone()));
            }
            Ok(ProviderResponse {
                output_text: self.reply.clone(),
                tool_calls: Vec::new(),
            })
        }
    }

    struct StreamingProvider {
        events: Vec<StreamEvent>,
    }

    impl StreamingProvider {
        fn new(events: Vec<StreamEvent>) -> Self {
            Self { events }
        }
    }

    #[async_trait]
    impl Provider for StreamingProvider {
        async fn generate(
            &self,
            _system_prompt: &str,
            _history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            Ok(ProviderResponse {
                output_text: None,
                tool_calls: Vec::new(),
            })
        }

        async fn generate_stream(
            &self,
            _system_prompt: &str,
            _history: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<ProviderStream, FrameworkError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let events = self.events.clone();
            tokio::spawn(async move {
                for event in events {
                    match &event {
                        StreamEvent::Done => {
                            let _ = tx.send(event);
                            break;
                        }
                        _ => {
                            let _ = tx.send(event);
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
            });
            Ok(Box::pin(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
        }
    }

    struct ForwardProvider {
        inner: Arc<dyn Provider>,
    }

    #[async_trait]
    impl Provider for ForwardProvider {
        async fn generate(
            &self,
            system_prompt: &str,
            history: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<ProviderResponse, FrameworkError> {
            self.inner.generate(system_prompt, history, tools).await
        }

        async fn generate_stream(
            &self,
            system_prompt: &str,
            history: &[Message],
            tools: &[ToolDefinition],
        ) -> Result<ProviderStream, FrameworkError> {
            self.inner
                .generate_stream(system_prompt, history, tools)
                .await
        }
    }

    struct NoopInvoker;

    #[async_trait]
    impl AgentInvoker for NoopInvoker {
        async fn invoke_agent(
            &self,
            _request: AgentInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool("unexpected invoke_agent".to_owned()))
        }

        async fn invoke_worker(
            &self,
            _request: crate::tools::WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Err(FrameworkError::Tool("unexpected invoke_worker".to_owned()))
        }
    }

    struct LifecycleChannel {
        outbound: Mutex<Vec<(String, String)>>,
        outbound_with_id: Mutex<Vec<(String, String, String)>>,
        edits: Mutex<Vec<(String, String, String)>>,
        typing_events: Mutex<Vec<String>>,
        fail_typing: AtomicBool,
        fail_send: AtomicBool,
        fail_edit: AtomicBool,
        editable: AtomicBool,
        message_char_limit: AtomicUsize,
        send_delay_ms: AtomicUsize,
        edit_delay_ms: AtomicUsize,
        next_message_id: AtomicUsize,
    }

    impl Default for LifecycleChannel {
        fn default() -> Self {
            Self {
                outbound: Mutex::new(Vec::new()),
                outbound_with_id: Mutex::new(Vec::new()),
                edits: Mutex::new(Vec::new()),
                typing_events: Mutex::new(Vec::new()),
                fail_typing: AtomicBool::new(false),
                fail_send: AtomicBool::new(false),
                fail_edit: AtomicBool::new(false),
                editable: AtomicBool::new(true),
                message_char_limit: AtomicUsize::new(0),
                send_delay_ms: AtomicUsize::new(0),
                edit_delay_ms: AtomicUsize::new(0),
                next_message_id: AtomicUsize::new(0),
            }
        }
    }

    impl LifecycleChannel {
        fn with_typing_failure() -> Self {
            Self {
                fail_typing: AtomicBool::new(true),
                ..Default::default()
            }
        }

        fn with_send_failure() -> Self {
            Self {
                fail_send: AtomicBool::new(true),
                ..Default::default()
            }
        }

        fn with_edit_failure() -> Self {
            Self {
                fail_edit: AtomicBool::new(true),
                ..Default::default()
            }
        }

        fn non_editable() -> Self {
            Self {
                editable: AtomicBool::new(false),
                ..Default::default()
            }
        }

        fn with_message_limit(limit: usize) -> Self {
            Self {
                message_char_limit: AtomicUsize::new(limit),
                ..Default::default()
            }
        }

        fn with_send_delay(ms: usize) -> Self {
            Self {
                send_delay_ms: AtomicUsize::new(ms),
                ..Default::default()
            }
        }

        fn with_edit_delay(ms: usize) -> Self {
            Self {
                edit_delay_ms: AtomicUsize::new(ms),
                ..Default::default()
            }
        }

        async fn outbound(&self) -> Vec<(String, String)> {
            self.outbound.lock().await.clone()
        }

        async fn outbound_with_id(&self) -> Vec<(String, String, String)> {
            self.outbound_with_id.lock().await.clone()
        }

        async fn edits(&self) -> Vec<(String, String, String)> {
            self.edits.lock().await.clone()
        }

        async fn typing_events(&self) -> Vec<String> {
            self.typing_events.lock().await.clone()
        }
    }

    #[async_trait]
    impl Channel for LifecycleChannel {
        fn supports_message_editing(&self) -> bool {
            self.editable.load(Ordering::Relaxed)
        }

        fn message_char_limit(&self) -> Option<usize> {
            match self.message_char_limit.load(Ordering::Relaxed) {
                0 => None,
                limit => Some(limit),
            }
        }

        async fn send_message(
            &self,
            channel_id: &str,
            content: &str,
        ) -> Result<(), FrameworkError> {
            self.outbound
                .lock()
                .await
                .push((channel_id.to_owned(), content.to_owned()));
            if self.fail_send.load(Ordering::Relaxed) {
                return Err(FrameworkError::Tool("simulated send failure".to_owned()));
            }
            Ok(())
        }

        async fn add_reaction(
            &self,
            _channel_id: &str,
            _message_id: &str,
            _emoji: &str,
        ) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn broadcast_typing(&self, channel_id: &str) -> Result<(), FrameworkError> {
            self.typing_events.lock().await.push(channel_id.to_owned());
            if self.fail_typing.load(Ordering::Relaxed) {
                return Err(FrameworkError::Tool("simulated typing failure".to_owned()));
            }
            Ok(())
        }

        async fn send_message_with_id(
            &self,
            channel_id: &str,
            content: &str,
        ) -> Result<Option<String>, FrameworkError> {
            if !self.supports_message_editing() {
                self.send_message(channel_id, content).await?;
                return Ok(None);
            }
            let delay_ms = self.send_delay_ms.load(Ordering::Relaxed) as u64;
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let message_id = format!(
                "stream-msg-{}",
                self.next_message_id.fetch_add(1, Ordering::Relaxed)
            );
            self.outbound_with_id.lock().await.push((
                channel_id.to_owned(),
                message_id.clone(),
                content.to_owned(),
            ));
            if self.fail_send.load(Ordering::Relaxed) {
                return Err(FrameworkError::Tool("simulated send failure".to_owned()));
            }
            Ok(Some(message_id))
        }

        async fn edit_message(
            &self,
            channel_id: &str,
            message_id: &str,
            content: &str,
        ) -> Result<(), FrameworkError> {
            let delay_ms = self.edit_delay_ms.load(Ordering::Relaxed) as u64;
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            self.edits.lock().await.push((
                channel_id.to_owned(),
                message_id.to_owned(),
                content.to_owned(),
            ));
            if self.fail_edit.load(Ordering::Relaxed) {
                return Err(FrameworkError::Tool("simulated edit failure".to_owned()));
            }
            Ok(())
        }

        async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
            pending::<Result<ChannelInbound, FrameworkError>>().await
        }
    }

    #[async_trait]
    impl Channel for AckCaptureChannel {
        async fn send_message(
            &self,
            _channel_id: &str,
            _content: &str,
        ) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn add_reaction(
            &self,
            channel_id: &str,
            message_id: &str,
            emoji: &str,
        ) -> Result<(), FrameworkError> {
            self.reactions.lock().await.push((
                channel_id.to_owned(),
                message_id.to_owned(),
                emoji.to_owned(),
            ));
            if self.fail_reaction.load(Ordering::Relaxed) {
                return Err(FrameworkError::Tool(
                    "simulated reaction failure".to_owned(),
                ));
            }
            Ok(())
        }

        async fn broadcast_typing(&self, _channel_id: &str) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
            pending::<Result<ChannelInbound, FrameworkError>>().await
        }
    }

    fn inbound_message() -> InboundMessage {
        InboundMessage {
            trace_id: next_trace_id(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "default".to_owned(),
            session_key: "agent:default:discord:chan-1".to_owned(),
            source_message_id: Some("msg-1".to_owned()),
            channel_id: "chan-1".to_owned(),
            guild_id: None,
            is_dm: false,
            user_id: "user-1".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: true,
            invoke: true,
            content: "hello".to_owned(),
        }
    }

    fn test_gateway(channel: Arc<dyn Channel>) -> Gateway {
        let mut channels = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel);
        Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        )
    }

    fn lifecycle_runtime_state(
        channel: Arc<LifecycleChannel>,
        provider: Arc<dyn Provider>,
        memory: DynMemory,
        output_mode: ChannelOutputMode,
    ) -> RuntimeState {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel);
        let (gateway_tx, _gateway_rx) = mpsc::channel(4);
        let gateway = Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, output_mode)]),
            RoutingConfig::default(),
        ));
        let mut agent_config = AgentInnerConfig::default();
        agent_config.tools = agent_config.tools.with_disabled(&["cron"]);
        let runtime_config = AgentRuntimeConfig {
            agent_id: "default".to_owned(),
            provider_key: "default".to_owned(),
            effective_execution: ExecutionDefaultsConfig::default(),
            owner_ids: vec!["user-1".to_owned()],
            agent_config,
            persona_root: PathBuf::from("/tmp/simpleclaw-run-test-persona"),
            workspace_root: PathBuf::from("/tmp/simpleclaw-run-test"),
            app_base_dir: PathBuf::from("/tmp/simpleclaw-run-test-app"),
            system_prompt: "base prompt".to_owned(),
        };
        let react_loop = Arc::new(ReactLoop::new(
            ProviderFactory::from_parts(HashMap::from([(
                "default".to_owned(),
                (
                    Box::new(ForwardProvider { inner: provider }) as Box<dyn Provider>,
                    true,
                ),
            )])),
            default_factory(),
            SkillFactory::new(PathBuf::from("/tmp/simpleclaw-run-skill-test")),
            Arc::new(NoopInvoker),
        ));

        let agents = Arc::new(AgentDirectory::new(
            HashMap::from([("default".to_owned(), runtime_config.clone())]),
            HashMap::from([("default".to_owned(), memory)]),
        ));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic enough for tests")
            .as_nanos();
        let cron_path = std::env::temp_dir().join(format!("simpleclaw_run_cron_{nanos}.db"));
        let context = Arc::new(RuntimeContext {
            react_loop,
            gateway: Arc::clone(&gateway),
            agents,
            tool_runtime: Arc::new(ToolRuntime {
                process_manager: Arc::new(ProcessManager::new()),
                completion_tx: gateway_tx.clone(),
            }),
        });

        RuntimeState {
            gateway,
            runtimes: HashMap::from([("default".to_owned(), AgentRuntime::new(runtime_config))]),
            context,
            cron_store: Arc::new(std::sync::Mutex::new(
                crate::tools::builtin::cron::CronStore::open(&cron_path)
                    .expect("cron store should open"),
            )),
            safe_error_reply: "safe fallback".to_owned(),
        }
    }

    fn test_handler() -> (
        SessionHandler<InboundMessage>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let handler: SessionHandler<InboundMessage> = Arc::new(move |inbound: InboundMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(inbound.trace_id);
            })
        });
        (handler, rx)
    }

    #[tokio::test]
    async fn dispatch_inbound_adds_seen_reaction_for_invoke_messages() {
        let channel = Arc::new(AckCaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let (handler, mut processed_rx) = test_handler();
        let inbound = inbound_message();

        dispatch_inbound_with_ack(&coordinator, handler, &gateway, inbound.clone()).await;

        let processed = timeout(Duration::from_secs(1), processed_rx.recv())
            .await
            .expect("message should be processed")
            .expect("processed trace id should exist");
        assert_eq!(processed, inbound.trace_id);
        let reactions = channel.reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, inbound.channel_id);
        assert_eq!(
            reactions[0].1,
            inbound.source_message_id.unwrap_or_default()
        );
        assert_eq!(reactions[0].2, INBOUND_ACK_REACTION);
    }

    #[tokio::test]
    async fn dispatch_inbound_skips_seen_reaction_for_passive_messages() {
        let channel = Arc::new(AckCaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let (handler, mut processed_rx) = test_handler();
        let mut inbound = inbound_message();
        inbound.invoke = false;

        dispatch_inbound_with_ack(&coordinator, handler, &gateway, inbound).await;

        let _ = timeout(Duration::from_secs(1), processed_rx.recv())
            .await
            .expect("message should be processed");
        assert!(channel.reactions().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_inbound_skips_seen_reaction_for_system_messages() {
        let channel = Arc::new(AckCaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let (handler, mut processed_rx) = test_handler();
        let mut inbound = inbound_message();
        inbound.user_id = "system".to_owned();

        dispatch_inbound_with_ack(&coordinator, handler, &gateway, inbound).await;

        let _ = timeout(Duration::from_secs(1), processed_rx.recv())
            .await
            .expect("message should be processed");
        assert!(channel.reactions().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_inbound_continues_when_seen_reaction_fails() {
        let channel = Arc::new(AckCaptureChannel::with_reaction_failure());
        let gateway = test_gateway(channel.clone());
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let (handler, mut processed_rx) = test_handler();
        let inbound = inbound_message();

        dispatch_inbound_with_ack(&coordinator, handler, &gateway, inbound).await;

        let _ = timeout(Duration::from_secs(1), processed_rx.recv())
            .await
            .expect("message should still be processed");
        assert_eq!(channel.reactions().await.len(), 1);
    }

    #[tokio::test]
    async fn handle_inbound_once_sends_config_error_for_unknown_agent() {
        let channel = Arc::new(LifecycleChannel::default());
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel.clone());
        let (gateway_tx, _gateway_rx) = mpsc::channel(4);
        let gateway = Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        ));
        let context = Arc::new(RuntimeContext {
            react_loop: Arc::new(ReactLoop::new(
                ProviderFactory::from_parts(HashMap::new()),
                default_factory(),
                SkillFactory::new(PathBuf::from("/tmp/simpleclaw-run-skill-test")),
                Arc::new(NoopInvoker),
            )),
            gateway: Arc::clone(&gateway),
            agents: Arc::new(AgentDirectory::new(HashMap::new(), HashMap::new())),
            tool_runtime: Arc::new(ToolRuntime {
                process_manager: Arc::new(ProcessManager::new()),
                completion_tx: gateway_tx.clone(),
            }),
        });
        let state = RuntimeState {
            gateway,
            runtimes: HashMap::new(),
            context,
            cron_store: Arc::new(std::sync::Mutex::new(
                crate::tools::builtin::cron::CronStore::open(
                    &std::env::temp_dir().join("simpleclaw_run_unknown_agent_cron.db"),
                )
                .expect("cron store should open"),
            )),
            safe_error_reply: "safe fallback".to_owned(),
        };
        let inbound = inbound_message();

        handle_inbound_once(&state, inbound)
            .await
            .expect("handler should not fail");

        let outbound = channel.outbound().await;
        assert_eq!(outbound.len(), 1);
        assert_eq!(
            outbound[0].1,
            "I couldn't route that message due to a configuration error."
        );
    }

    #[tokio::test]
    async fn handle_inbound_once_records_passive_context_without_outbound_reply() {
        let channel = Arc::new(LifecycleChannel::default());
        let memory_impl = Arc::new(FakeMemory::default());
        let memory: DynMemory = memory_impl.clone();
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("unused"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );
        let mut inbound = inbound_message();
        inbound.invoke = false;

        handle_inbound_once(&state, inbound.clone())
            .await
            .expect("handler should succeed");

        let appended = memory_impl.appended().await;
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].1, StoredRole::User);
        assert_eq!(appended[0].2, inbound.content);
        assert!(channel.outbound().await.is_empty());
        assert!(channel.typing_events().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_continues_after_typing_failure_and_sends_reply() {
        let channel = Arc::new(LifecycleChannel::with_typing_failure());
        let memory_impl = Arc::new(FakeMemory::default());
        let memory: DynMemory = memory_impl.clone();
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("hello back"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        assert_eq!(channel.typing_events().await.len(), 1);
        let outbound = channel.outbound_with_id().await;
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0].2, "hello back");
        assert!(channel.edits().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_sends_safe_reply_when_runtime_fails() {
        let channel = Arc::new(LifecycleChannel::default());
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::err("boom"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should swallow runtime failure");

        let streamed = channel.outbound_with_id().await;
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].2, "safe fallback");
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_swallows_channel_send_failure_after_successful_run() {
        let channel = Arc::new(LifecycleChannel::with_send_failure());
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("reply"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed despite send failure");

        let streamed = channel.outbound_with_id().await;
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].2, "reply");
        let outbound = channel.outbound().await;
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0].1, "reply");
    }

    #[tokio::test]
    async fn handle_inbound_once_uses_streaming_message_send_and_final_edit() {
        let channel = Arc::new(LifecycleChannel::default());
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("streamed reply"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        let initial = channel.outbound_with_id().await;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].2, "streamed reply");
        assert!(channel.edits().await.is_empty());
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_waits_for_slow_initial_stream_send_without_duplicate_fallback() {
        let channel = Arc::new(LifecycleChannel::with_send_delay(150));
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("slow streamed reply"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        let initial = channel.outbound_with_id().await;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].2, "slow streamed reply");
        assert!(channel.edits().await.is_empty());
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_uses_single_final_send_for_non_editable_channels() {
        let channel = Arc::new(LifecycleChannel::non_editable());
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("plain reply"));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        let outbound = channel.outbound().await;
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0].1, "plain reply");
        assert!(channel.outbound_with_id().await.is_empty());
        assert!(channel.edits().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_uses_single_final_send_when_output_mode_is_normal() {
        let channel = Arc::new(LifecycleChannel::default());
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StaticProvider::ok("plain reply"));
        let state =
            lifecycle_runtime_state(channel.clone(), provider, memory, ChannelOutputMode::Normal);

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        let outbound = channel.outbound().await;
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0].1, "plain reply");
        assert!(channel.outbound_with_id().await.is_empty());
        assert!(channel.edits().await.is_empty());
    }

    #[tokio::test]
    async fn handle_inbound_once_finalizes_after_in_flight_edit_without_duplicate_send() {
        let channel = Arc::new(LifecycleChannel::with_edit_delay(150));
        let memory: DynMemory = Arc::new(FakeMemory::default());
        let provider: Arc<dyn Provider> = Arc::new(StreamingProvider::new(vec![
            StreamEvent::TextDelta("hel".to_owned()),
            StreamEvent::TextDelta("lo".to_owned()),
            StreamEvent::Done,
        ]));
        let state = lifecycle_runtime_state(
            channel.clone(),
            provider,
            memory,
            ChannelOutputMode::Streaming,
        );

        handle_inbound_once(&state, inbound_message())
            .await
            .expect("handler should succeed");

        let initial = channel.outbound_with_id().await;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].2, "hel");
        let edits = channel.edits().await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, initial[0].1);
        assert_eq!(edits[0].2, "hello");
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn streaming_display_rolls_over_and_edits_new_segment_after_limit() {
        let channel = Arc::new(LifecycleChannel::with_message_limit(2_000));
        let first = "a".repeat(1_990);
        let second = "b".repeat(30);
        let third = "c".repeat(5);
        let gateway = Arc::new(test_gateway(channel.clone()));
        let display = Arc::new(std::sync::Mutex::new(StreamingDisplay::new(
            gateway,
            inbound_message(),
            Some(2_000),
        )));

        spawn_streaming_display_update(&display, &first);
        timeout(Duration::from_secs(1), async {
            loop {
                if channel.outbound_with_id().await.len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first segment should send");

        tokio::time::sleep(STREAMING_EDIT_INTERVAL).await;
        spawn_streaming_display_update(&display, &format!("{first}{second}"));
        timeout(Duration::from_secs(1), async {
            loop {
                if channel.outbound_with_id().await.len() == 2 && channel.edits().await.len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("rollover should edit the first segment and send the overflow tail");

        finalize_streaming_display(&display, &format!("{first}{second}{third}"))
            .await
            .expect("finalize should succeed");

        let initial = channel.outbound_with_id().await;
        assert_eq!(initial.len(), 2);
        assert_eq!(initial[0].2, first);
        assert_eq!(initial[1].2, "b".repeat(20));

        let edits = channel.edits().await;
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].1, initial[0].1);
        assert_eq!(
            edits[0].2,
            format!("{}{}", "a".repeat(1_990), "b".repeat(10))
        );
        assert_eq!(edits[1].1, initial[1].1);
        assert_eq!(edits[1].2, format!("{}{}", "b".repeat(20), third));
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn finalize_streaming_display_rolls_over_when_final_content_exceeds_limit() {
        let channel = Arc::new(LifecycleChannel::with_message_limit(2_000));
        let gateway = Arc::new(test_gateway(channel.clone()));
        let first = "a".repeat(1_990);
        let final_content = format!("{}{}", first, "b".repeat(110));
        let display = Arc::new(std::sync::Mutex::new(StreamingDisplay {
            gateway,
            inbound: inbound_message(),
            channel_limit: Some(2_000),
            latest_content: first.clone(),
            committed_prefix_chars: 0,
            displayed_segment: Some(first.clone()),
            message_id: Some("stream-msg-0".to_owned()),
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            initial_send_attempted: true,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
        }));

        finalize_streaming_display(&display, &final_content)
            .await
            .expect("finalize should succeed");

        let initial = channel.outbound_with_id().await;
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].2, "b".repeat(100));

        let edits = channel.edits().await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, "stream-msg-0");
        assert_eq!(
            edits[0].2,
            format!("{}{}", "a".repeat(1_990), "b".repeat(10))
        );
    }

    #[tokio::test]
    async fn finalize_streaming_display_stops_after_terminal_edit_failure() {
        let channel = Arc::new(LifecycleChannel::with_edit_failure());
        let gateway = Arc::new(test_gateway(channel.clone()));
        let display = Arc::new(std::sync::Mutex::new(StreamingDisplay {
            gateway,
            inbound: inbound_message(),
            channel_limit: Some(2_000),
            latest_content: "hello".to_owned(),
            committed_prefix_chars: 0,
            displayed_segment: Some("hello".to_owned()),
            message_id: Some("stream-msg-0".to_owned()),
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            initial_send_attempted: true,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
        }));

        let result = timeout(
            Duration::from_secs(1),
            finalize_streaming_display(&display, "hello world"),
        )
        .await
        .expect("finalizer should not spin forever");

        assert!(result.is_err());
        let edits = channel.edits().await;
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].2, "hello world");
        assert!(channel.outbound_with_id().await.is_empty());
        assert!(channel.outbound().await.is_empty());
    }

    #[tokio::test]
    async fn streaming_display_rate_limits_edit_spawns() {
        let channel = Arc::new(LifecycleChannel::default());
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel.clone());
        let gateway = Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        ));
        let display = Arc::new(std::sync::Mutex::new(StreamingDisplay {
            gateway,
            inbound: inbound_message(),
            channel_limit: None,
            latest_content: "first".to_owned(),
            committed_prefix_chars: 0,
            displayed_segment: Some("stale".to_owned()),
            message_id: Some("stream-msg-1".to_owned()),
            last_edit: Instant::now(),
            initial_send_attempted: true,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
        }));

        spawn_streaming_display_update(&display, "first");
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert!(channel.edits().await.is_empty());
    }
}
