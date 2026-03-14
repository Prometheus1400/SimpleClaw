use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{Instrument, debug, info_span};

use crate::channels::InboundMessage;
use crate::error::FrameworkError;
use crate::sandbox::PreparedHostCommand;

use super::CompletionRoute;

#[derive(Debug, Clone)]
pub(crate) struct StartedAsyncToolRun {
    pub run_id: String,
    pub tool_name: String,
    pub kind: AsyncToolRunKind,
}

impl StartedAsyncToolRun {
    pub(crate) fn accepted_output(&self) -> String {
        serde_json::json!({
            "status": "accepted",
            "runId": self.run_id,
            "tool": self.tool_name,
            "kind": self.kind.as_str(),
        })
        .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncToolRunKind {
    Process,
    Delegated,
}

impl AsyncToolRunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Delegated => "delegated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncToolRunStatus {
    Running,
    Completed,
    Killed,
}

impl AsyncToolRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AsyncToolRunSnapshot {
    pub run_id: String,
    pub tool_name: String,
    pub kind: AsyncToolRunKind,
    pub status: AsyncToolRunStatus,
    pub summary: String,
    pub details: AsyncToolRunDetails,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub enum AsyncToolRunDetails {
    Process(ProcessAsyncToolRunDetails),
    Delegated(DelegatedAsyncToolRunDetails),
}

#[derive(Debug, Clone)]
pub struct ProcessAsyncToolRunDetails {
    pub command: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct DelegatedAsyncToolRunDetails {
    pub request: String,
    pub reply: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct AsyncToolRunManager {
    runs: Mutex<HashMap<String, AsyncToolRunEntry>>,
    counter: AtomicU64,
}

impl AsyncToolRunManager {
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    pub(crate) async fn start_process(
        self: &Arc<Self>,
        tool_name: &str,
        command: &str,
        agent_id: &str,
        session_key: &str,
        workspace_root: Option<&std::path::Path>,
        env: &BTreeMap<String, String>,
        completion_tx: Option<mpsc::Sender<InboundMessage>>,
        route: Option<CompletionRoute>,
    ) -> Result<StartedAsyncToolRun, FrameworkError> {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("async-tool-run-{}-{seq}", Utc::now().timestamp_millis());
        let base = std::env::temp_dir().join("simpleclaw_process");
        std::fs::create_dir_all(&base)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create temp dir: {e}")))?;
        let stdout_path = base.join(format!("{run_id}.stdout.log"));
        let stderr_path = base.join(format!("{run_id}.stderr.log"));
        let stdout_file = File::create(&stdout_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stdout log: {e}")))?;
        let stderr_file = File::create(&stderr_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stderr log: {e}")))?;

        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-lc").arg(command);
        cmd.envs(env);
        if let Some(workspace_root) = workspace_root {
            cmd.current_dir(workspace_root);
        }
        cmd.stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        let child = cmd
            .spawn()
            .map_err(|e| FrameworkError::Tool(format!("exec failed to start: {e}")))?;

        let started_at = Utc::now();
        let pid = Some(child.id());
        let handle = CompletionHandle::Host(child);
        let entry = AsyncToolRunEntry {
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Process,
            summary: command.to_owned(),
            agent_id: agent_id.to_owned(),
            session_key: session_key.to_owned(),
            status: AsyncToolRunStatus::Running,
            started_at,
            finished_at: None,
            completion_notify: Arc::new(Notify::new()),
            claimed: false,
            process: Some(ProcessRunState {
                command: command.to_owned(),
                pid,
                stdout_path,
                stderr_path,
                exit_code: None,
            }),
            delegated: None,
        };

        let mut runs = self.runs.lock().await;
        runs.insert(run_id.clone(), entry);
        Self::auto_evict(&mut runs);
        drop(runs);
        debug!(status = "started", "async tool run");
        self.spawn_completion_watcher(run_id.clone(), handle, completion_tx, route);
        Ok(StartedAsyncToolRun {
            run_id,
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Process,
        })
    }

    fn auto_evict(runs: &mut HashMap<String, AsyncToolRunEntry>) {
        const MAX_COMPLETED: usize = 64;
        let completed_count = runs
            .values()
            .filter(|e| e.status != AsyncToolRunStatus::Running)
            .count();
        if completed_count <= MAX_COMPLETED {
            return;
        }
        let mut completed: Vec<(String, DateTime<Utc>)> = runs
            .iter()
            .filter(|(_, e)| e.status != AsyncToolRunStatus::Running)
            .map(|(id, e)| (id.clone(), e.finished_at.unwrap_or(e.started_at)))
            .collect();
        completed.sort_by_key(|(_, t)| *t);
        let to_evict = completed_count - MAX_COMPLETED;
        for (id, _) in completed.into_iter().take(to_evict) {
            if let Some(entry) = runs.remove(&id) {
                entry.cleanup_files();
            }
        }
    }

    pub(crate) async fn start_prepared_process(
        self: &Arc<Self>,
        tool_name: &str,
        command: &str,
        agent_id: &str,
        session_key: &str,
        prepared: PreparedHostCommand,
        env: &BTreeMap<String, String>,
        completion_tx: Option<mpsc::Sender<InboundMessage>>,
        route: Option<CompletionRoute>,
    ) -> Result<StartedAsyncToolRun, FrameworkError> {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("async-tool-run-{}-{seq}", Utc::now().timestamp_millis());
        let base = std::env::temp_dir().join("simpleclaw_process");
        std::fs::create_dir_all(&base)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create temp dir: {e}")))?;
        let stdout_path = base.join(format!("{run_id}.stdout.log"));
        let stderr_path = base.join(format!("{run_id}.stderr.log"));
        let stdout_file = File::create(&stdout_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stdout log: {e}")))?;
        let stderr_file = File::create(&stderr_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stderr log: {e}")))?;
        let spawned = prepared.spawn(env, Stdio::from(stdout_file), Stdio::from(stderr_file))?;

        let started_at = Utc::now();
        let pid = Some(spawned.pid());
        let handle = CompletionHandle::HostSandboxed(spawned);
        let entry = AsyncToolRunEntry {
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Process,
            summary: command.to_owned(),
            agent_id: agent_id.to_owned(),
            session_key: session_key.to_owned(),
            status: AsyncToolRunStatus::Running,
            started_at,
            finished_at: None,
            completion_notify: Arc::new(Notify::new()),
            claimed: false,
            process: Some(ProcessRunState {
                command: command.to_owned(),
                pid,
                stdout_path,
                stderr_path,
                exit_code: None,
            }),
            delegated: None,
        };

        let mut runs = self.runs.lock().await;
        runs.insert(run_id.clone(), entry);
        Self::auto_evict(&mut runs);
        drop(runs);
        debug!(status = "started", "async tool run");
        self.spawn_completion_watcher(run_id.clone(), handle, completion_tx, route);
        Ok(StartedAsyncToolRun {
            run_id,
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Process,
        })
    }

    pub(crate) async fn start_delegated<F>(
        self: &Arc<Self>,
        tool_name: &str,
        request: &str,
        agent_id: &str,
        session_key: &str,
        completion_tx: Option<mpsc::Sender<InboundMessage>>,
        route: Option<CompletionRoute>,
        future: F,
    ) -> Result<StartedAsyncToolRun, FrameworkError>
    where
        F: Future<Output = Result<String, FrameworkError>> + Send + 'static,
    {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("async-tool-run-{}-{seq}", Utc::now().timestamp_millis());
        let started_at = Utc::now();
        let entry = AsyncToolRunEntry {
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Delegated,
            summary: request.to_owned(),
            agent_id: agent_id.to_owned(),
            session_key: session_key.to_owned(),
            status: AsyncToolRunStatus::Running,
            started_at,
            finished_at: None,
            completion_notify: Arc::new(Notify::new()),
            claimed: false,
            process: None,
            delegated: Some(DelegatedRunState {
                request: request.to_owned(),
                reply: None,
                error: None,
            }),
        };

        let mut runs = self.runs.lock().await;
        runs.insert(run_id.clone(), entry);
        Self::auto_evict(&mut runs);
        drop(runs);
        debug!(status = "started", "async tool run");

        let join_handle = tokio::spawn(future);
        self.spawn_completion_watcher(
            run_id.clone(),
            CompletionHandle::Delegated(join_handle),
            completion_tx,
            route,
        );
        Ok(StartedAsyncToolRun {
            run_id,
            tool_name: tool_name.to_owned(),
            kind: AsyncToolRunKind::Delegated,
        })
    }

    pub async fn get(&self, run_id: &str) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get_mut(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown async tool run_id: {run_id}")))?;
        entry.poll_completion();
        Ok(entry.snapshot(run_id.to_owned()))
    }

    pub async fn get_for_session(
        &self,
        run_id: &str,
        agent_id: &str,
        session_key: &str,
    ) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get_mut(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown background run_id: {run_id}")))?;
        entry.poll_completion();
        if !entry.belongs_to(agent_id, session_key) {
            return Err(FrameworkError::Tool(format!(
                "unknown background run_id: {run_id}"
            )));
        }
        Ok(entry.snapshot(run_id.to_owned()))
    }

    pub async fn list_for_session(
        &self,
        agent_id: &str,
        session_key: &str,
    ) -> Vec<AsyncToolRunSnapshot> {
        let metadata: Vec<AsyncToolRunEntryMeta> = {
            let mut runs = self.runs.lock().await;
            for entry in runs.values_mut() {
                entry.poll_completion();
            }
            runs.iter()
                .filter(|(_, entry)| entry.belongs_to(agent_id, session_key))
                .map(|(id, entry)| entry.metadata(id.clone()))
                .collect()
        };
        let mut items: Vec<AsyncToolRunSnapshot> = metadata
            .into_iter()
            .map(AsyncToolRunEntryMeta::into_snapshot)
            .collect();
        items.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        items
    }

    pub async fn kill_for_session(
        &self,
        run_id: &str,
        agent_id: &str,
        session_key: &str,
    ) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get_mut(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown background run_id: {run_id}")))?;
        if !entry.belongs_to(agent_id, session_key) {
            return Err(FrameworkError::Tool(format!(
                "unknown background run_id: {run_id}"
            )));
        }
        entry.kill().await?;
        Ok(entry.snapshot(run_id.to_owned()))
    }

    pub async fn wait_for_session(
        &self,
        run_ids: &[String],
        agent_id: &str,
        session_key: &str,
        timeout_ms: u64,
    ) -> Result<Vec<AsyncToolRunSnapshot>, FrameworkError> {
        let claimed = self.claim_runs(run_ids, agent_id, session_key).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let pending: Vec<(String, Arc<Notify>)> = claimed
            .iter()
            .filter(|(_, _, already_completed)| !already_completed)
            .map(|(run_id, notify, _)| (run_id.clone(), Arc::clone(notify)))
            .collect();

        futures::future::join_all(pending.iter().map(|(_, notify)| {
            let notify = Arc::clone(notify);
            async move {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let _ = tokio::time::timeout(remaining, notify.notified()).await;
            }
        }))
        .await;

        let mut still_running = Vec::new();
        for run_id in run_ids {
            let snapshot = self.get_for_session(run_id, agent_id, session_key).await?;
            if snapshot.status == AsyncToolRunStatus::Running {
                still_running.push(run_id.clone());
            }
        }
        self.unclaim_runs(&still_running).await;

        let mut snapshots = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            snapshots.push(self.get_for_session(run_id, agent_id, session_key).await?);
        }
        Ok(snapshots)
    }

    fn spawn_completion_watcher(
        self: &Arc<Self>,
        run_id: String,
        handle: CompletionHandle,
        completion_tx: Option<mpsc::Sender<InboundMessage>>,
        route: Option<CompletionRoute>,
    ) {
        let pm = Arc::clone(self);
        let trace_id = route
            .as_ref()
            .map(|r| r.trace_id.clone())
            .unwrap_or_else(|| "no-trace".to_owned());
        let route_session = route
            .as_ref()
            .map(|r| r.session_key.clone())
            .unwrap_or_else(|| run_id.clone());
        let span = info_span!(
            "async_tool_run.completion_watcher",
            trace_id = %trace_id,
            session_id = %route_session
        );
        tokio::spawn(
            async move {
                let content = match handle {
                    CompletionHandle::Host(mut child) => {
                        let status = tokio::task::spawn_blocking(move || child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok());
                        let exit_code = status.and_then(|s| s.code());
                        pm.mark_process_completed(&run_id, exit_code).await;
                        pm.completion_content_for_process(&run_id, exit_code).await
                    }
                    CompletionHandle::HostSandboxed(spawned) => {
                        let (mut child, cleanup) = spawned.into_parts();
                        let status = tokio::task::spawn_blocking(move || child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok());
                        let exit_code = status.and_then(|s| s.code());
                        cleanup.cleanup().await;
                        pm.mark_process_completed(&run_id, exit_code).await;
                        pm.completion_content_for_process(&run_id, exit_code).await
                    }
                    CompletionHandle::Delegated(join_handle) => {
                        let result = match join_handle.await {
                            Ok(outcome) => outcome,
                            Err(err) => Err(FrameworkError::Tool(format!(
                                "delegated async run join failed: {err}"
                            ))),
                        };
                        pm.mark_delegated_completed(&run_id, &result).await;
                        pm.completion_content_for_delegated(&run_id, &result).await
                    }
                };

                if pm.should_reinject(&run_id).await {
                    if let (Some(tx), Some(route)) = (completion_tx, route) {
                        let msg = InboundMessage {
                            trace_id: route.trace_id.clone(),
                            source_channel: route.source_channel,
                            target_agent_id: route.target_agent_id,
                            session_key: route.session_key,
                            source_message_id: None,
                            channel_id: route.channel_id,
                            guild_id: route.guild_id,
                            is_dm: route.is_dm,
                            user_id: "system".to_owned(),
                            username: "system".to_owned(),
                            mentioned_bot: false,
                            invoke: true,
                            content,
                        };
                        if let Err(err) = tx.send(msg).await {
                            tracing::warn!(
                                status = "failed",
                                error_kind = "completion_send",
                                error = %err,
                                "failed to send async tool run completion message"
                            );
                        } else {
                            debug!(status = "completed", "async tool run completion watcher");
                        }
                    } else {
                        debug!(
                            status = "completed_no_route",
                            "async tool run completion watcher"
                        );
                    }
                } else {
                    debug!(
                        status = "completed_claimed",
                        "async tool run completion watcher"
                    );
                }
            }
            .instrument(span),
        );
    }

    async fn mark_process_completed(&self, run_id: &str, exit_code: Option<i32>) {
        let mut runs = self.runs.lock().await;
        if let Some(entry) = runs.get_mut(run_id)
            && entry.status == AsyncToolRunStatus::Running
        {
            entry.status = AsyncToolRunStatus::Completed;
            if let Some(process) = entry.process.as_mut() {
                process.exit_code = exit_code;
            }
            entry.finished_at = Some(Utc::now());
            entry.completion_notify.notify_waiters();
        }
    }

    async fn mark_delegated_completed(
        &self,
        run_id: &str,
        result: &Result<String, FrameworkError>,
    ) {
        let mut runs = self.runs.lock().await;
        if let Some(entry) = runs.get_mut(run_id)
            && entry.status == AsyncToolRunStatus::Running
        {
            entry.status = AsyncToolRunStatus::Completed;
            entry.finished_at = Some(Utc::now());
            if let Some(delegated) = entry.delegated.as_mut() {
                match result {
                    Ok(reply) => {
                        delegated.reply = Some(reply.clone());
                        delegated.error = None;
                    }
                    Err(err) => {
                        delegated.reply = None;
                        delegated.error = Some(err.to_string());
                    }
                }
            }
            entry.completion_notify.notify_waiters();
        }
    }

    async fn claim_runs(
        &self,
        run_ids: &[String],
        agent_id: &str,
        session_key: &str,
    ) -> Result<Vec<(String, Arc<Notify>, bool)>, FrameworkError> {
        let mut runs = self.runs.lock().await;
        run_ids
            .iter()
            .map(|run_id| {
                let entry = runs.get_mut(run_id).ok_or_else(|| {
                    FrameworkError::Tool(format!("unknown background run_id: {run_id}"))
                })?;
                if !entry.belongs_to(agent_id, session_key) {
                    return Err(FrameworkError::Tool(format!(
                        "unknown background run_id: {run_id}"
                    )));
                }
                entry.claimed = true;
                Ok((
                    run_id.clone(),
                    Arc::clone(&entry.completion_notify),
                    entry.status != AsyncToolRunStatus::Running,
                ))
            })
            .collect()
    }

    async fn unclaim_runs(&self, run_ids: &[String]) {
        let mut runs = self.runs.lock().await;
        for run_id in run_ids {
            if let Some(entry) = runs.get_mut(run_id)
                && entry.status == AsyncToolRunStatus::Running
            {
                entry.claimed = false;
            }
        }
    }

    async fn should_reinject(&self, run_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(run_id)
            .map(|entry| !entry.claimed)
            .unwrap_or(false)
    }

    async fn completion_content_for_process(&self, run_id: &str, exit_code: Option<i32>) -> String {
        let summary = self
            .get(run_id)
            .await
            .ok()
            .map(|s| s.summary)
            .unwrap_or_else(|| "unknown".to_owned());
        format!(
            "[async tool run completed] run_id={} kind=process exit_code={} summary={}",
            run_id,
            exit_code.unwrap_or(-1),
            summary
        )
    }

    async fn completion_content_for_delegated(
        &self,
        run_id: &str,
        result: &Result<String, FrameworkError>,
    ) -> String {
        let snapshot = self.get(run_id).await.ok();
        let (tool_name, summary) = snapshot
            .map(|s| (s.tool_name, s.summary))
            .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
        match result {
            Ok(_) => format!(
                "[async tool run completed] run_id={} tool={} kind=delegated status=ok summary={}",
                run_id, tool_name, summary
            ),
            Err(err) => format!(
                "[async tool run completed] run_id={} tool={} kind=delegated status=error error={} summary={}",
                run_id, tool_name, err, summary
            ),
        }
    }
}

impl Default for AsyncToolRunManager {
    fn default() -> Self {
        Self::new()
    }
}

enum CompletionHandle {
    Host(std::process::Child),
    HostSandboxed(crate::sandbox::SpawnedHostCommand),
    Delegated(tokio::task::JoinHandle<Result<String, FrameworkError>>),
}

#[derive(Debug)]
struct AsyncToolRunEntry {
    tool_name: String,
    kind: AsyncToolRunKind,
    summary: String,
    agent_id: String,
    session_key: String,
    status: AsyncToolRunStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    completion_notify: Arc<Notify>,
    claimed: bool,
    process: Option<ProcessRunState>,
    delegated: Option<DelegatedRunState>,
}

#[derive(Debug)]
struct ProcessRunState {
    command: String,
    pid: Option<u32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    exit_code: Option<i32>,
}

#[derive(Debug)]
struct DelegatedRunState {
    request: String,
    reply: Option<String>,
    error: Option<String>,
}

struct AsyncToolRunEntryMeta {
    run_id: String,
    tool_name: String,
    kind: AsyncToolRunKind,
    summary: String,
    status: AsyncToolRunStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    process: Option<ProcessRunStateMeta>,
    delegated: Option<DelegatedRunStateMeta>,
}

struct ProcessRunStateMeta {
    command: String,
    pid: Option<u32>,
    exit_code: Option<i32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

struct DelegatedRunStateMeta {
    request: String,
    reply: Option<String>,
    error: Option<String>,
}

impl AsyncToolRunEntry {
    fn poll_completion(&mut self) {
        // Host processes are tracked by the completion watcher which owns the Child.
        // No on-demand poll is possible here.
    }

    fn belongs_to(&self, agent_id: &str, session_key: &str) -> bool {
        self.agent_id == agent_id && self.session_key == session_key
    }

    async fn kill(&mut self) -> Result<(), FrameworkError> {
        if self.status != AsyncToolRunStatus::Running {
            return Ok(());
        }
        let Some(process) = self.process.as_mut() else {
            return Err(FrameworkError::Tool(
                "background run kind does not support kill".to_owned(),
            ));
        };
        if let Some(pid) = process.pid {
            let output = Command::new("kill")
                .arg(pid.to_string())
                .output()
                .await
                .map_err(|e| FrameworkError::Tool(format!("failed to kill background run: {e}")))?;
            if !output.status.success() {
                tracing::warn!(
                    status = "failed",
                    error_kind = "kill_background_run",
                    "background run kill"
                );
            }
        }
        self.status = AsyncToolRunStatus::Killed;
        self.finished_at = Some(Utc::now());
        process.exit_code = Some(-1);
        self.completion_notify.notify_waiters();
        Ok(())
    }

    fn metadata(&self, run_id: String) -> AsyncToolRunEntryMeta {
        AsyncToolRunEntryMeta {
            run_id,
            tool_name: self.tool_name.clone(),
            kind: self.kind,
            summary: self.summary.clone(),
            status: self.status.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            process: self.process.as_ref().map(|process| ProcessRunStateMeta {
                command: process.command.clone(),
                pid: process.pid,
                exit_code: process.exit_code,
                stdout_path: process.stdout_path.clone(),
                stderr_path: process.stderr_path.clone(),
            }),
            delegated: self
                .delegated
                .as_ref()
                .map(|delegated| DelegatedRunStateMeta {
                    request: delegated.request.clone(),
                    reply: delegated.reply.clone(),
                    error: delegated.error.clone(),
                }),
        }
    }

    fn snapshot(&self, run_id: String) -> AsyncToolRunSnapshot {
        let details = if let Some(process) = self.process.as_ref() {
            AsyncToolRunDetails::Process(ProcessAsyncToolRunDetails {
                command: process.command.clone(),
                pid: process.pid,
                exit_code: process.exit_code,
                stdout: read_process_output_tail(&process.stdout_path, 32_768),
                stderr: read_process_output_tail(&process.stderr_path, 16_384),
            })
        } else if let Some(delegated) = self.delegated.as_ref() {
            AsyncToolRunDetails::Delegated(DelegatedAsyncToolRunDetails {
                request: delegated.request.clone(),
                reply: delegated.reply.clone(),
                error: delegated.error.clone(),
            })
        } else {
            AsyncToolRunDetails::Delegated(DelegatedAsyncToolRunDetails {
                request: self.summary.clone(),
                reply: None,
                error: Some("run details unavailable".to_owned()),
            })
        };
        AsyncToolRunSnapshot {
            run_id,
            tool_name: self.tool_name.clone(),
            kind: self.kind,
            status: self.status.clone(),
            summary: self.summary.clone(),
            details,
            started_at: self.started_at,
            finished_at: self.finished_at,
        }
    }

    fn cleanup_files(self) {
        if let Some(process) = self.process {
            let _ = std::fs::remove_file(&process.stdout_path);
            let _ = std::fs::remove_file(&process.stderr_path);
        }
    }
}

impl AsyncToolRunEntryMeta {
    fn into_snapshot(self) -> AsyncToolRunSnapshot {
        let details = if let Some(process) = self.process {
            AsyncToolRunDetails::Process(ProcessAsyncToolRunDetails {
                command: process.command,
                pid: process.pid,
                exit_code: process.exit_code,
                stdout: read_process_output_tail(&process.stdout_path, 32_768),
                stderr: read_process_output_tail(&process.stderr_path, 16_384),
            })
        } else if let Some(delegated) = self.delegated {
            AsyncToolRunDetails::Delegated(DelegatedAsyncToolRunDetails {
                request: delegated.request,
                reply: delegated.reply,
                error: delegated.error,
            })
        } else {
            AsyncToolRunDetails::Delegated(DelegatedAsyncToolRunDetails {
                request: self.summary.clone(),
                reply: None,
                error: Some("run details unavailable".to_owned()),
            })
        };
        AsyncToolRunSnapshot {
            run_id: self.run_id,
            tool_name: self.tool_name,
            kind: self.kind,
            status: self.status,
            summary: self.summary,
            details,
            started_at: self.started_at,
            finished_at: self.finished_at,
        }
    }
}

impl Drop for AsyncToolRunManager {
    fn drop(&mut self) {}
}

fn read_process_output_tail(path: &std::path::Path, max_bytes: u64) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    if len > max_bytes {
        let _ = file.seek(SeekFrom::Start(len - max_bytes));
    } else {
        let _ = file.seek(SeekFrom::Start(0));
    }
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use tokio::time::{Duration, sleep};

    use super::{AsyncToolRunDetails, AsyncToolRunManager, AsyncToolRunStatus};

    #[tokio::test]
    async fn completion_watcher_marks_background_process_complete_without_route() {
        let manager = Arc::new(AsyncToolRunManager::new());
        let started = manager
            .start_process(
                "exec",
                "echo hello",
                "default",
                "session-1",
                None,
                &BTreeMap::new(),
                None,
                None,
            )
            .await
            .expect("spawn should succeed");

        for _ in 0..20 {
            let snapshot = manager
                .get(&started.run_id)
                .await
                .expect("get should succeed");
            if snapshot.status != AsyncToolRunStatus::Running {
                assert_eq!(snapshot.status, AsyncToolRunStatus::Completed);
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }

        panic!("background run did not complete in time");
    }

    #[tokio::test]
    async fn list_for_session_excludes_other_sessions() {
        let manager = Arc::new(AsyncToolRunManager::new());
        manager
            .start_process(
                "exec",
                "echo hello",
                "agent-a",
                "session-a",
                None,
                &BTreeMap::new(),
                None,
                None,
            )
            .await
            .expect("spawn should succeed");
        manager
            .start_process(
                "exec",
                "echo hello",
                "agent-a",
                "session-b",
                None,
                &BTreeMap::new(),
                None,
                None,
            )
            .await
            .expect("spawn should succeed");

        let items = manager.list_for_session("agent-a", "session-a").await;
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn delegated_runs_appear_in_list_and_capture_reply() {
        let manager = Arc::new(AsyncToolRunManager::new());
        let started = manager
            .start_delegated(
                "task",
                "do delegated work",
                "agent-a",
                "session-a",
                None,
                None,
                async { Ok("done".to_owned()) },
            )
            .await
            .expect("start delegated should succeed");

        for _ in 0..20 {
            let snapshot = manager
                .get(&started.run_id)
                .await
                .expect("snapshot should exist");
            if snapshot.status != AsyncToolRunStatus::Running {
                assert_eq!(snapshot.status, AsyncToolRunStatus::Completed);
                let AsyncToolRunDetails::Delegated(details) = snapshot.details else {
                    panic!("delegated run should return delegated details");
                };
                assert_eq!(details.reply.as_deref(), Some("done"));
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("delegated run did not complete in time");
    }

    #[tokio::test]
    async fn delegated_runs_cannot_be_killed() {
        let manager = Arc::new(AsyncToolRunManager::new());
        let started = manager
            .start_delegated(
                "summon",
                "handoff",
                "agent-a",
                "session-a",
                None,
                None,
                async {
                    sleep(Duration::from_millis(200)).await;
                    Ok("done".to_owned())
                },
            )
            .await
            .expect("start delegated should succeed");

        let err = manager
            .kill_for_session(&started.run_id, "agent-a", "session-a")
            .await
            .err()
            .expect("kill should fail for delegated runs");
        assert!(
            err.to_string()
                .contains("background run kind does not support kill")
        );
    }

    #[tokio::test]
    async fn wait_for_session_returns_completed_snapshot() {
        let manager = Arc::new(AsyncToolRunManager::new());
        let started = manager
            .start_delegated(
                "task",
                "do delegated work",
                "agent-a",
                "session-a",
                None,
                None,
                async {
                    sleep(Duration::from_millis(25)).await;
                    Ok("done".to_owned())
                },
            )
            .await
            .expect("start delegated should succeed");

        let snapshots = manager
            .wait_for_session(
                std::slice::from_ref(&started.run_id),
                "agent-a",
                "session-a",
                1_000,
            )
            .await
            .expect("wait should succeed");

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].status, AsyncToolRunStatus::Completed);
    }
}
