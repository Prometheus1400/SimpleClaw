use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
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
}

impl AsyncToolRunKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Process => "process",
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
    pub process: Option<ProcessAsyncToolRunDetails>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ProcessAsyncToolRunDetails {
    pub command: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
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

    pub async fn start_process(
        self: &Arc<Self>,
        tool_name: &str,
        command: &str,
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
            command: command.to_owned(),
            status: AsyncToolRunStatus::Running,
            started_at,
            finished_at: None,
            pid,
            stdout_path,
            stderr_path,
            exit_code: None,
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

    pub async fn start_prepared_process(
        self: &Arc<Self>,
        tool_name: &str,
        command: &str,
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
            command: command.to_owned(),
            status: AsyncToolRunStatus::Running,
            started_at,
            finished_at: None,
            pid,
            stdout_path,
            stderr_path,
            exit_code: None,
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

    pub async fn get(&self, run_id: &str) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get_mut(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown async tool run_id: {run_id}")))?;
        entry.poll_completion();
        Ok(entry.snapshot(run_id.to_owned()))
    }

    pub async fn list(&self) -> Vec<AsyncToolRunSnapshot> {
        let metadata: Vec<AsyncToolRunEntryMeta> = {
            let mut runs = self.runs.lock().await;
            for entry in runs.values_mut() {
                entry.poll_completion();
            }
            runs.iter()
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

    pub async fn cancel(&self, run_id: &str) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get_mut(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown async tool run_id: {run_id}")))?;
        entry.kill().await?;
        Ok(entry.snapshot(run_id.to_owned()))
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
                let (exit_code, sandbox_manager) = match handle {
                    CompletionHandle::Host(mut child) => {
                        let status = tokio::task::spawn_blocking(move || child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok());
                        (status.and_then(|s| s.code()), None)
                    }
                    CompletionHandle::HostSandboxed(spawned) => {
                        let (mut child, cleanup) = spawned.into_parts();
                        let status = tokio::task::spawn_blocking(move || child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok());
                        (status.and_then(|s| s.code()), Some(cleanup))
                    }
                };

                if let Some(cleanup) = sandbox_manager {
                    cleanup.cleanup().await;
                }

                pm.mark_completed(&run_id, exit_code).await;

                let snapshot = pm.get(&run_id).await;
                let summary = snapshot
                    .as_ref()
                    .map(|s| s.summary.as_str())
                    .unwrap_or("unknown");
                let code = exit_code.unwrap_or(-1);
                let content = format!(
                    "[async tool run completed] run_id={} tool=exec kind=process exit_code={} summary={}",
                    run_id, code, summary
                );
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
                    debug!(status = "completed_no_route", "async tool run completion watcher");
                }
            }
            .instrument(span),
        );
    }

    async fn mark_completed(&self, run_id: &str, exit_code: Option<i32>) {
        let mut runs = self.runs.lock().await;
        if let Some(entry) = runs.get_mut(run_id)
            && entry.status == AsyncToolRunStatus::Running
        {
            entry.status = AsyncToolRunStatus::Completed;
            entry.exit_code = exit_code;
            entry.finished_at = Some(Utc::now());
        }
    }

    pub async fn forget(&self, run_id: &str) -> Result<AsyncToolRunSnapshot, FrameworkError> {
        let mut runs = self.runs.lock().await;
        let entry = runs
            .get(run_id)
            .ok_or_else(|| FrameworkError::Tool(format!("unknown async tool run_id: {run_id}")))?;
        if entry.status == AsyncToolRunStatus::Running {
            return Err(FrameworkError::Tool(
                "cannot forget a running async tool run; cancel it first".to_owned(),
            ));
        }
        let entry = runs.remove(run_id).unwrap();
        let snapshot = entry.snapshot(run_id.to_owned());
        entry.cleanup_files();
        Ok(snapshot)
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
}

#[derive(Debug)]
struct AsyncToolRunEntry {
    tool_name: String,
    kind: AsyncToolRunKind,
    summary: String,
    command: String,
    status: AsyncToolRunStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    pid: Option<u32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    exit_code: Option<i32>,
}

struct AsyncToolRunEntryMeta {
    run_id: String,
    tool_name: String,
    kind: AsyncToolRunKind,
    summary: String,
    command: String,
    status: AsyncToolRunStatus,
    pid: Option<u32>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl AsyncToolRunEntry {
    fn poll_completion(&mut self) {
        // Host processes are tracked by the completion watcher which owns the Child.
        // No on-demand poll is possible here.
    }

    async fn kill(&mut self) -> Result<(), FrameworkError> {
        if self.status != AsyncToolRunStatus::Running {
            return Ok(());
        }
        if let Some(pid) = self.pid {
            let output = Command::new("kill")
                .arg(pid.to_string())
                .output()
                .await
                .map_err(|e| FrameworkError::Tool(format!("failed to kill process: {e}")))?;
            if !output.status.success() {
                tracing::warn!(
                    status = "failed",
                    error_kind = "kill_process",
                    "process kill"
                );
            }
        }
        self.status = AsyncToolRunStatus::Killed;
        self.finished_at = Some(Utc::now());
        self.exit_code = Some(-1);
        Ok(())
    }

    fn metadata(&self, run_id: String) -> AsyncToolRunEntryMeta {
        AsyncToolRunEntryMeta {
            run_id,
            tool_name: self.tool_name.clone(),
            kind: self.kind,
            summary: self.summary.clone(),
            command: self.command.clone(),
            status: self.status.clone(),
            pid: self.pid,
            started_at: self.started_at,
            finished_at: self.finished_at,
            exit_code: self.exit_code,
            stdout_path: self.stdout_path.clone(),
            stderr_path: self.stderr_path.clone(),
        }
    }

    fn snapshot(&self, run_id: String) -> AsyncToolRunSnapshot {
        AsyncToolRunSnapshot {
            run_id,
            tool_name: self.tool_name.clone(),
            kind: self.kind,
            status: self.status.clone(),
            summary: self.summary.clone(),
            process: Some(ProcessAsyncToolRunDetails {
                command: self.command.clone(),
                pid: self.pid,
                exit_code: self.exit_code,
                stdout: read_process_output_tail(&self.stdout_path, 32_768),
                stderr: read_process_output_tail(&self.stderr_path, 16_384),
            }),
            started_at: self.started_at,
            finished_at: self.finished_at,
        }
    }

    fn cleanup_files(self) {
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

impl AsyncToolRunEntryMeta {
    fn into_snapshot(self) -> AsyncToolRunSnapshot {
        AsyncToolRunSnapshot {
            run_id: self.run_id,
            tool_name: self.tool_name,
            kind: self.kind,
            status: self.status,
            summary: self.summary,
            process: Some(ProcessAsyncToolRunDetails {
                command: self.command,
                pid: self.pid,
                exit_code: self.exit_code,
                stdout: read_process_output_tail(&self.stdout_path, 32_768),
                stderr: read_process_output_tail(&self.stderr_path, 16_384),
            }),
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

    use super::{AsyncToolRunManager, AsyncToolRunStatus};

    #[tokio::test]
    async fn completion_watcher_marks_background_process_complete_without_route() {
        let manager = Arc::new(AsyncToolRunManager::new());
        let started = manager
            .start_process("exec", "echo hello", None, &BTreeMap::new(), None, None)
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

        panic!("background process did not complete in time");
    }
}
