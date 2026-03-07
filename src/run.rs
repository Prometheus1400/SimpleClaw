use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate};
use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tracing::info;

use crate::agent::{
    AgentRuntime, build_tool_registry_for_agent, load_agent_config_for_workspace,
    load_system_prompt_for_workspace,
};
use crate::channel::{Channel, DiscordChannel, InboundMessage, LoggingChannel};
use crate::cli::{Cli, MemoryMode};
use crate::config::{AgentEntryConfig, GatewayChannelKind, LoadedConfig, ProviderKind};
use crate::gateway::Gateway;
use crate::memory::MemoryStore;
use crate::paths::AppPaths;
use crate::provider::{GeminiProvider, Provider};

pub const RETAIN_DAILY_LOG_FILES: usize = 2;
const SESSION_WORKER_IDLE_TIMEOUT_SECS: u64 = 300;

#[derive(Clone)]
pub struct RotatingLogWriter {
    inner: Arc<Mutex<RotatingLogState>>,
}

struct RotatingLogState {
    log_path: PathBuf,
    current_day: NaiveDate,
    retain_daily_files: usize,
    file: File,
}

impl RotatingLogWriter {
    pub fn new(log_path: PathBuf, retain_daily_files: usize) -> color_eyre::Result<Self> {
        let today = Local::now().date_naive();
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .wrap_err("failed to open active log file")?;
        rotate_stale_active_log_if_needed(&log_path, today)
            .wrap_err("failed to rotate stale active log file")?;
        prune_daily_logs(&log_path, retain_daily_files).wrap_err("failed to prune daily logs")?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .wrap_err("failed to reopen active log file")?;
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingLogState {
                log_path,
                current_day: today,
                retain_daily_files,
                file,
            })),
        })
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock rotating log writer"))?;
        state.rotate_if_needed()?;
        state.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock rotating log writer"))?;
        state.file.flush()
    }
}

impl RotatingLogState {
    fn rotate_if_needed(&mut self) -> std::io::Result<()> {
        let today = Local::now().date_naive();
        if today == self.current_day {
            return Ok(());
        }
        self.file.flush()?;
        rotate_active_log_to_day(&self.log_path, self.current_day)?;
        prune_daily_logs(&self.log_path, self.retain_daily_files)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        self.current_day = today;
        Ok(())
    }
}

fn rotate_stale_active_log_if_needed(log_path: &Path, today: NaiveDate) -> std::io::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }
    let metadata = fs::metadata(log_path)?;
    if metadata.len() == 0 {
        return Ok(());
    }
    let modified_day = DateTime::<Local>::from(metadata.modified()?).date_naive();
    if modified_day < today {
        rotate_active_log_to_day(log_path, modified_day)?;
    }
    Ok(())
}

fn rotate_active_log_to_day(log_path: &Path, day: NaiveDate) -> std::io::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }
    let target = dated_log_path(log_path, day);
    if target.exists() {
        let mut src = OpenOptions::new().read(true).open(log_path)?;
        let mut dst = OpenOptions::new().create(true).append(true).open(&target)?;
        std::io::copy(&mut src, &mut dst)?;
        fs::remove_file(log_path)?;
    } else {
        fs::rename(log_path, target)?;
    }
    Ok(())
}

fn prune_daily_logs(log_path: &Path, retain_daily_files: usize) -> std::io::Result<()> {
    let mut daily_logs = list_daily_log_files(log_path)?;
    if daily_logs.len() <= retain_daily_files {
        return Ok(());
    }
    daily_logs.sort_by_key(|(day, _)| *day);
    let remove_count = daily_logs.len().saturating_sub(retain_daily_files);
    for (_, path) in daily_logs.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn list_daily_log_files(log_path: &Path) -> std::io::Result<Vec<(NaiveDate, PathBuf)>> {
    let Some(dir) = log_path.parent() else {
        return Ok(Vec::new());
    };
    let Some(base_name) = log_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(Vec::new());
    };

    let prefix = format!("{base_name}.");
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let raw_date = &name[prefix.len()..];
        if let Ok(day) = NaiveDate::parse_from_str(raw_date, "%Y-%m-%d") {
            out.push((day, path));
        }
    }
    Ok(out)
}

fn dated_log_path(log_path: &Path, day: NaiveDate) -> PathBuf {
    let name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("service.log");
    log_path.with_file_name(format!("{name}.{}", day.format("%Y-%m-%d")))
}

fn collect_log_history(log_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut history = list_daily_log_files(log_path)?;
    history.sort_by_key(|(day, _)| *day);
    let mut out = history
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    if log_path.exists() {
        out.push(log_path.to_path_buf());
    }
    Ok(out)
}

#[async_trait]
pub(crate) trait ProviderFactory: Send + Sync {
    async fn create_provider(&self, loaded: &LoadedConfig)
    -> color_eyre::Result<Arc<dyn Provider>>;
}

#[async_trait]
pub(crate) trait MemoryFactory: Send + Sync {
    async fn create_memory(
        &self,
        agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<MemoryStore>;
}

#[async_trait]
pub(crate) trait ChannelFactory: Send + Sync {
    async fn create_channels(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>>;
}

pub(crate) struct RuntimeDependencies {
    pub provider_factory: Arc<dyn ProviderFactory>,
    pub memory_factory: Arc<dyn MemoryFactory>,
    pub channel_factory: Arc<dyn ChannelFactory>,
}

impl Default for RuntimeDependencies {
    fn default() -> Self {
        Self {
            provider_factory: Arc::new(DefaultProviderFactory),
            memory_factory: Arc::new(DefaultMemoryFactory),
            channel_factory: Arc::new(DefaultChannelFactory),
        }
    }
}

struct DefaultProviderFactory;

#[async_trait]
impl ProviderFactory for DefaultProviderFactory {
    async fn create_provider(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<Arc<dyn Provider>> {
        let provider: Arc<dyn Provider> = match loaded.global.provider.kind {
            ProviderKind::Gemini => {
                Arc::new(GeminiProvider::from_config(loaded.global.provider.clone()))
            }
        };
        Ok(provider)
    }
}

struct DefaultMemoryFactory;

#[async_trait]
impl MemoryFactory for DefaultMemoryFactory {
    async fn create_memory(
        &self,
        agent: &AgentEntryConfig,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<MemoryStore> {
        let (memory_dir, short_term_path, long_term_path) =
            agent_workspace_memory_paths(&agent.workspace);
        fs::create_dir_all(&memory_dir).wrap_err_with(|| {
            format!(
                "failed to create memory directory for agent '{}': {}",
                agent.id,
                memory_dir.display()
            )
        })?;
        MemoryStore::new(
            &short_term_path,
            &long_term_path,
            &loaded.global.database,
            &loaded.global.embedding,
        )
        .await
        .wrap_err_with(|| {
            format!(
                "failed to initialize sqlite memory store for agent '{}'",
                agent.id
            )
        })
    }
}

struct DefaultChannelFactory;

#[async_trait]
impl ChannelFactory for DefaultChannelFactory {
    async fn create_channels(
        &self,
        loaded: &LoadedConfig,
    ) -> color_eyre::Result<HashMap<GatewayChannelKind, Arc<dyn Channel>>> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        for kind in &loaded.global.gateway.channels {
            let channel: Arc<dyn Channel> = match kind {
                GatewayChannelKind::Discord => Arc::new(
                    DiscordChannel::from_config(&loaded.global.discord)
                        .await
                        .wrap_err("failed to initialize discord channel")?,
                ),
                GatewayChannelKind::Logging => {
                    Arc::new(LoggingChannel::new(loaded.global.agents.default.clone()))
                }
            };
            channels.insert(*kind, channel);
        }
        Ok(channels)
    }
}

pub(crate) struct RuntimeState {
    pub gateway: Gateway,
    pub runtimes: HashMap<String, AgentRuntime>,
    pub safe_error_reply: String,
}

type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send>>;
type SessionHandler<T> = Arc<dyn Fn(T) -> BoxFutureUnit + Send + Sync>;

#[derive(Clone)]
struct SessionWorkerCoordinator<T> {
    workers: Arc<AsyncMutex<HashMap<String, SessionWorker<T>>>>,
    next_worker_id: Arc<AtomicU64>,
    idle_timeout: Duration,
}

#[derive(Clone)]
struct SessionWorker<T> {
    id: u64,
    tx: mpsc::UnboundedSender<T>,
}

impl<T> SessionWorkerCoordinator<T>
where
    T: Send + 'static,
{
    fn new(idle_timeout: Duration) -> Self {
        Self {
            workers: Arc::new(AsyncMutex::new(HashMap::new())),
            next_worker_id: Arc::new(AtomicU64::new(1)),
            idle_timeout,
        }
    }

    async fn dispatch(&self, key: String, message: T, handler: SessionHandler<T>) {
        let mut pending = Some(message);

        for attempt in 0..=1 {
            let (worker_id, tx) = self.worker_sender_for_key(&key, Arc::clone(&handler)).await;
            let payload = pending
                .take()
                .expect("pending message is always present before send attempt");

            match tx.send(payload) {
                Ok(()) => return,
                Err(err) => {
                    pending = Some(err.0);
                    self.remove_worker_if_matches(&key, worker_id).await;

                    if attempt == 1 {
                        tracing::error!(
                            session_key = %key,
                            "dropping inbound message after worker enqueue retries were exhausted"
                        );
                        return;
                    }
                }
            }
        }
    }

    async fn worker_sender_for_key(
        &self,
        key: &str,
        handler: SessionHandler<T>,
    ) -> (u64, mpsc::UnboundedSender<T>) {
        let mut workers = self.workers.lock().await;
        if let Some(existing) = workers.get(key) {
            return (existing.id, existing.tx.clone());
        }

        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        workers.insert(
            key.to_owned(),
            SessionWorker {
                id: worker_id,
                tx: tx.clone(),
            },
        );
        drop(workers);

        self.spawn_worker(key.to_owned(), worker_id, rx, handler);
        (worker_id, tx)
    }

    fn spawn_worker(
        &self,
        key: String,
        worker_id: u64,
        mut rx: mpsc::UnboundedReceiver<T>,
        handler: SessionHandler<T>,
    ) {
        let workers = Arc::clone(&self.workers);
        let idle_timeout = self.idle_timeout;
        tokio::spawn(async move {
            loop {
                let next = tokio::time::timeout(idle_timeout, rx.recv()).await;
                let Some(message) = (match next {
                    Ok(Some(message)) => Some(message),
                    Ok(None) => None,
                    Err(_) => {
                        tracing::debug!(session_key = %key, "session worker idled out");
                        None
                    }
                }) else {
                    break;
                };

                handler(message).await;
            }

            let mut workers = workers.lock().await;
            if workers.get(&key).is_some_and(|entry| entry.id == worker_id) {
                workers.remove(&key);
            }
        });
    }

    async fn remove_worker_if_matches(&self, key: &str, expected_worker_id: u64) {
        let mut workers = self.workers.lock().await;
        if workers
            .get(key)
            .is_some_and(|entry| entry.id == expected_worker_id)
        {
            workers.remove(key);
        }
    }

    #[cfg(test)]
    async fn worker_count(&self) -> usize {
        self.workers.lock().await.len()
    }
}

pub(crate) async fn assemble_runtime_state(
    cli: &Cli,
    loaded: &LoadedConfig,
    app_paths: &AppPaths,
    deps: &RuntimeDependencies,
) -> color_eyre::Result<RuntimeState> {
    let provider = deps.provider_factory.create_provider(loaded).await?;

    let mut memory_by_agent: HashMap<String, MemoryStore> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = deps.memory_factory.create_memory(agent, loaded).await?;
        memory_by_agent.insert(agent.id.clone(), memory);
    }

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::channel::<InboundMessage>(1_024);

    let summon_agents: HashMap<String, std::path::PathBuf> = loaded
        .global
        .agents
        .list
        .iter()
        .map(|agent| (agent.id.clone(), agent.workspace.clone()))
        .collect();
    let mut runtimes: HashMap<String, AgentRuntime> = HashMap::new();
    for agent in &loaded.global.agents.list {
        let memory = memory_by_agent.get(&agent.id).cloned().ok_or_else(|| {
            color_eyre::eyre::eyre!("missing memory store for configured agent '{}'", agent.id)
        })?;
        let agent_config = load_agent_config_for_workspace(&agent.workspace)
            .wrap_err_with(|| format!("failed to load agent.yaml for agent '{}'", agent.id))?;
        let system_prompt =
            load_system_prompt_for_workspace(&agent.workspace).wrap_err_with(|| {
                format!(
                    "failed to assemble layered system prompt for agent '{}'",
                    agent.id
                )
            })?;
        let tooling = build_tool_registry_for_agent(
            &agent.id,
            &agent_config,
            &agent.workspace,
            &app_paths.base_dir,
        )
        .wrap_err_with(|| format!("failed to load skill tools for agent '{}'", agent.id))?;
        info!(
            agent_id = %agent.id,
            requested_skills = tooling.skill_stats.requested,
            loaded_skill_tools = tooling.skill_stats.loaded,
            skipped_missing_skills = tooling.skill_stats.skipped_missing,
            skipped_empty_skills = tooling.skill_stats.skipped_empty,
            "agent skill tools loaded"
        );
        runtimes.insert(
            agent.id.clone(),
            AgentRuntime::new(
                agent.id.clone(),
                loaded.global.runtime.clone(),
                agent_config,
                Arc::clone(&provider),
                loaded.global.provider.kind,
                memory,
                summon_agents.clone(),
                memory_by_agent.clone(),
                agent.workspace.clone(),
                app_paths.base_dir.clone(),
                system_prompt,
                tooling.tool_registry,
                tooling.skill_tool_names,
                cli.max_steps,
                Some(gateway_tx.clone()),
            ),
        );
    }

    let channels = deps.channel_factory.create_channels(loaded).await?;
    let gateway = Gateway::new(channels, gateway_tx, gateway_rx);

    Ok(RuntimeState {
        gateway,
        runtimes,
        safe_error_reply: loaded.global.runtime.safe_error_reply.clone(),
    })
}

pub(crate) async fn handle_inbound_once(
    state: &RuntimeState,
    inbound: InboundMessage,
) -> color_eyre::Result<()> {
    let memory_session_id = inbound.session_key.clone();
    let Some(runtime) = state.runtimes.get(&inbound.target_agent_id) else {
        tracing::error!(
            target_agent_id = %inbound.target_agent_id,
            channel_id = %inbound.channel_id,
            "dropping message due to unknown routed agent"
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
            tracing::error!(error = %err, "failed to send unknown-agent route reply");
        }
        return Ok(());
    };

    if !inbound.invoke {
        tracing::debug!(
            session_id = %memory_session_id,
            channel_id = %inbound.channel_id,
            guild_id = inbound.guild_id.as_deref().unwrap_or("dm"),
            user_id = %inbound.user_id,
            "recording passive channel context message"
        );
        if let Err(err) = runtime.record_context(&inbound, &memory_session_id).await {
            tracing::error!(error = %err, "failed to persist passive context message");
        }
        return Ok(());
    }

    if inbound.user_id != "system"
        && let Err(err) = state.gateway.broadcast_typing(&inbound).await
    {
        tracing::warn!(error = %err, "failed to broadcast typing");
    }
    tracing::debug!(
        session_id = %memory_session_id,
        channel_id = %inbound.channel_id,
        guild_id = inbound.guild_id.as_deref().unwrap_or("dm"),
        is_dm = inbound.is_dm,
        user_id = %inbound.user_id,
        mentioned_bot = inbound.mentioned_bot,
        target_agent_id = %inbound.target_agent_id,
        "dispatching inbound message to agent"
    );

    match runtime.run(&inbound, &memory_session_id).await {
        Ok(reply) => {
            if let Err(err) = state.gateway.send_message(&inbound, &reply).await {
                tracing::error!(error = %err, "failed to send channel response");
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "agent execution failed");
            if let Err(send_err) = state
                .gateway
                .send_message(&inbound, &state.safe_error_reply)
                .await
            {
                tracing::error!(error = %send_err, "failed to send safe error reply");
            }
        }
    }
    Ok(())
}

pub async fn run_service(cli: &Cli) -> color_eyre::Result<()> {
    let app_paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    let loaded = LoadedConfig::load(cli.workspace.as_deref())
        .wrap_err("failed to load global/workspace configuration")?;
    let deps = RuntimeDependencies::default();
    let state = Arc::new(assemble_runtime_state(cli, &loaded, &app_paths, &deps).await?);
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

    info!(agent_count = state.runtimes.len(), "runtime initialized");
    loop {
        let inbound = match state.gateway.next_message().await {
            Ok(msg) => msg,
            Err(err) => {
                tracing::error!(error = %err, "gateway listen failed");
                continue;
            }
        };
        let key = inbound.session_key.clone();
        coordinator
            .dispatch(key, inbound, Arc::clone(&handler))
            .await;
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
    let mut child = ProcessCommand::new(exe);
    child
        .arg("--max-steps")
        .arg(cli.max_steps.to_string())
        .arg("system")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

fn state_paths(_cli: &Cli) -> color_eyre::Result<AppPaths> {
    AppPaths::resolve().map_err(Into::into)
}

fn agent_workspace_memory_paths(workspace: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let memory_dir = workspace.join(".simpleclaw").join("memory");
    let short_term_path = memory_dir.join("lraf.db");
    let long_term_path = memory_dir.join("lraf_long_term.db");
    (memory_dir, short_term_path, long_term_path)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::NaiveDate;
    use rusqlite::Connection;
    use tokio::sync::{Barrier, mpsc, oneshot};
    use tokio::time::{Duration, sleep, timeout};

    use super::{
        AsyncMutex, SessionHandler, SessionWorkerCoordinator, collect_log_history, dated_log_path,
        prune_daily_logs, query_long_memory, query_short_memory,
    };

    fn temp_db_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic enough for tests")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}.db"))
    }

    fn temp_log_path(prefix: &str) -> PathBuf {
        temp_db_path(prefix).with_extension("log")
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

    #[test]
    fn prune_daily_logs_keeps_latest_two_days() {
        let log_path = temp_log_path("prune_daily_logs");
        let first = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 4).expect("date should be valid"),
        );
        let second = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 5).expect("date should be valid"),
        );
        let third = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 6).expect("date should be valid"),
        );

        fs::write(&first, "oldest\n").expect("should create first dated log");
        fs::write(&second, "middle\n").expect("should create second dated log");
        fs::write(&third, "newest\n").expect("should create third dated log");

        prune_daily_logs(&log_path, 2).expect("pruning should succeed");

        assert!(!first.exists());
        assert!(second.exists());
        assert!(third.exists());

        let _ = fs::remove_file(&second);
        let _ = fs::remove_file(&third);
    }

    #[test]
    fn collect_log_history_orders_days_then_active() {
        let log_path = temp_log_path("history_order");
        let old = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 5).expect("date should be valid"),
        );
        let new = dated_log_path(
            &log_path,
            NaiveDate::from_ymd_opt(2026, 3, 6).expect("date should be valid"),
        );

        fs::write(&old, "older\n").expect("should create old dated log");
        fs::write(&new, "newer\n").expect("should create new dated log");
        fs::write(&log_path, "active\n").expect("should create active log");

        let history = collect_log_history(&log_path).expect("history should be discovered");
        assert_eq!(history, vec![old.clone(), new.clone(), log_path.clone()]);

        let _ = fs::remove_file(&old);
        let _ = fs::remove_file(&new);
        let _ = fs::remove_file(&log_path);
    }

    #[derive(Debug)]
    struct TestMessage {
        id: usize,
    }

    fn boxed_handler<F, Fut>(f: F) -> SessionHandler<TestMessage>
    where
        F: Fn(TestMessage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Arc::new(move |message| Box::pin(f(message)))
    }

    #[tokio::test]
    async fn session_workers_serialize_messages_for_same_key() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
        let (second_started_tx, second_started_rx) = oneshot::channel::<()>();
        let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
        let release_first_rx = Arc::new(AsyncMutex::new(Some(release_first_rx)));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<usize>();
        let first_started_tx = Arc::new(AsyncMutex::new(Some(first_started_tx)));
        let second_started_tx = Arc::new(AsyncMutex::new(Some(second_started_tx)));

        let handler = boxed_handler({
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let release_first_rx = Arc::clone(&release_first_rx);
            let done_tx = done_tx.clone();
            let first_started_tx = Arc::clone(&first_started_tx);
            let second_started_tx = Arc::clone(&second_started_tx);
            move |message: TestMessage| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let release_first_rx = Arc::clone(&release_first_rx);
                let done_tx = done_tx.clone();
                let first_started_tx = Arc::clone(&first_started_tx);
                let second_started_tx = Arc::clone(&second_started_tx);
                async move {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    loop {
                        let prev = max_active.load(Ordering::SeqCst);
                        if now_active <= prev {
                            break;
                        }
                        if max_active
                            .compare_exchange(prev, now_active, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break;
                        }
                    }

                    if message.id == 1 {
                        if let Some(tx) = first_started_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release_first_rx.lock().await.take() {
                            let _ = rx.await;
                        }
                    }

                    if message.id == 2
                        && let Some(tx) = second_started_tx.lock().await.take()
                    {
                        let _ = tx.send(());
                    }

                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = done_tx.send(message.id);
                }
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;

        first_started_rx
            .await
            .expect("first message should begin processing");
        assert!(
            timeout(Duration::from_millis(100), second_started_rx)
                .await
                .is_err(),
            "second message should not start before first is released"
        );

        let _ = release_first_tx.send(());
        let _ = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("first completion should arrive")
            .expect("first completion value should exist");
        let _ = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("second completion should arrive")
            .expect("second completion value should exist");

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn session_workers_run_different_keys_concurrently() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let barrier = Arc::new(Barrier::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let handler = boxed_handler({
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            move |_message: TestMessage| {
                let barrier = Arc::clone(&barrier);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                async move {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    loop {
                        let prev = max_active.load(Ordering::SeqCst);
                        if now_active <= prev {
                            break;
                        }
                        if max_active
                            .compare_exchange(prev, now_active, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break;
                        }
                    }
                    barrier.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        coordinator
            .dispatch(
                "session-b".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;

        timeout(Duration::from_secs(1), barrier.wait())
            .await
            .expect("both session workers should run concurrently");
        assert!(
            max_active.load(Ordering::SeqCst) >= 2,
            "expected at least two concurrent handlers"
        );
    }

    #[tokio::test]
    async fn session_worker_expires_when_idle_and_respawns() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_millis(50));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<usize>();
        let handler = boxed_handler(move |message: TestMessage| {
            let done_tx = done_tx.clone();
            async move {
                let _ = done_tx.send(message.id);
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        let first = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("first completion should arrive")
            .expect("first completion payload should exist");
        assert_eq!(first, 1);

        for _ in 0..20 {
            if coordinator.worker_count().await == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(coordinator.worker_count().await, 0);

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;
        let second = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("second completion should arrive")
            .expect("second completion payload should exist");
        assert_eq!(second, 2);
    }
}
