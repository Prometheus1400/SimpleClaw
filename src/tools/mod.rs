pub mod builtin;
pub mod sandbox;
pub mod sandbox_runtime;
pub mod skill;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{Instrument, debug, info_span};

use tokio::sync::mpsc;

use crate::channels::InboundMessage;
use crate::config::{GatewayChannelKind, ToolSandboxConfig, ToolsConfig};
use crate::dispatch::ToolExecutionResult;
use crate::error::FrameworkError;
use crate::gateway::Gateway;
use crate::memory::DynMemory;
use crate::providers::ToolDefinition;

#[derive(Debug, Clone)]
pub struct AgentInvokeRequest {
    pub target_agent_id: String,
    pub session_id: String,
    pub user_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct WorkerInvokeRequest {
    pub current_agent_id: String,
    pub session_id: String,
    pub user_id: String,
    pub prompt: String,
    pub max_steps_override: Option<u32>,
}

#[async_trait]
pub trait AgentInvoker: Send + Sync {
    async fn invoke_agent(
        &self,
        request: AgentInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError>;
    async fn invoke_worker(
        &self,
        request: WorkerInvokeRequest,
    ) -> Result<InvokeOutcome, FrameworkError>;
}

#[derive(Debug, Clone)]
pub struct InvokeOutcome {
    pub reply: String,
    pub tool_calls: Vec<ToolExecutionResult>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolRunOutput {
    pub output: String,
    pub nested_tool_calls: Vec<ToolExecutionResult>,
}

impl ToolRunOutput {
    pub(crate) fn plain(output: String) -> Self {
        Self {
            output,
            nested_tool_calls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRoute {
    pub trace_id: String,
    pub source_channel: GatewayChannelKind,
    pub target_agent_id: String,
    pub session_key: String,
    pub source_message_id: Option<String>,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub is_dm: bool,
}

#[derive(Clone)]
pub(crate) struct ToolExecEnv {
    pub agent_id: String,
    pub memory: DynMemory,
    pub workspace_root: PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub process_manager: Arc<ProcessManager>,
    pub invoker: Arc<dyn AgentInvoker>,
    pub gateway: Option<Arc<Gateway>>,
    pub completion_tx: Option<mpsc::Sender<InboundMessage>>,
    pub completion_route: Option<CompletionRoute>,
}

impl ToolExecEnv {
    pub fn owner_allowed(user_id: &str, owner_ids: &[String]) -> bool {
        !owner_ids.is_empty() && owner_ids.iter().any(|owner_id| owner_id == user_id)
    }

    pub fn is_owner(&self) -> bool {
        Self::owner_allowed(&self.user_id, &self.owner_ids)
    }
}

#[async_trait]
pub(crate) trait Tool: Send + Sync + ToolClone {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema_json(&self) -> &str;
    fn configure(&mut self, _config: Value) -> Result<(), FrameworkError> {
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError>;

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolRunOutput, FrameworkError> {
        self.execute(ctx, args_json, session_id)
            .await
            .map(ToolRunOutput::plain)
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            input_schema_json: self.input_schema_json().to_owned(),
        }
    }
}

pub(crate) trait ToolClone {
    fn box_clone(&self) -> Box<dyn Tool>;
}

impl<T> ToolClone for T
where
    T: Tool + Clone + 'static,
{
    fn box_clone(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}

#[derive(Default)]
pub(crate) struct ToolFactory {
    builtins: Vec<Box<dyn Tool>>,
    by_name: HashMap<String, Box<dyn Tool>>,
}

impl ToolFactory {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register_builtin(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_owned();
        if let Some(existing_idx) = self
            .builtins
            .iter()
            .position(|candidate| candidate.name() == name)
        {
            self.builtins[existing_idx] = tool.box_clone();
        } else {
            self.builtins.push(tool.box_clone());
        }
        self.by_name.insert(name, tool);
    }

    pub(crate) fn resolve_active(
        &self,
        tools_config: &ToolsConfig,
        skill_tools: &[Arc<dyn Tool>],
    ) -> Result<ActiveTools, FrameworkError> {
        let mut ordered = Vec::new();
        let mut by_name = HashMap::new();
        let mut owner_restricted_by_name = HashMap::new();
        let mut seen = HashSet::new();

        for name in tools_config.enabled_tool_names() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(tool_template) = self.by_name.get(&name) else {
                return Err(FrameworkError::Config(format!(
                    "unknown tool in tools map: {name}"
                )));
            };
            let mut tool = tool_template.box_clone();
            if let Some(config) = tools_config.config_for_tool(&name) {
                tool.configure(config)?;
            }
            let tool: Arc<dyn Tool> = Arc::from(tool);
            ordered.push(Arc::clone(&tool));
            by_name.insert(name.clone(), tool);
            let owner_restricted = tools_config
                .owner_restricted_for_tool(&name)
                .unwrap_or(true);
            owner_restricted_by_name.insert(name, owner_restricted);
        }

        for tool in skill_tools {
            let name = tool.name().to_owned();
            if !seen.insert(name.clone()) {
                continue;
            }
            ordered.push(Arc::clone(tool));
            by_name.insert(name, Arc::clone(tool));
            owner_restricted_by_name.insert(tool.name().to_owned(), false);
        }

        Ok(ActiveTools {
            ordered,
            by_name,
            owner_restricted_by_name,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ActiveTools {
    ordered: Vec<Arc<dyn Tool>>,
    by_name: HashMap<String, Arc<dyn Tool>>,
    owner_restricted_by_name: HashMap<String, bool>,
}

impl ActiveTools {
    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.ordered.iter().map(|tool| tool.definition()).collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub(crate) fn owner_restricted(&self, name: &str) -> bool {
        self.owner_restricted_by_name
            .get(name)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn without(&self, name: &str) -> Self {
        let ordered = self
            .ordered
            .iter()
            .filter(|tool| tool.name() != name)
            .cloned()
            .collect();
        let by_name = self
            .by_name
            .iter()
            .filter(|(tool_name, _)| tool_name.as_str() != name)
            .map(|(tool_name, tool)| (tool_name.clone(), Arc::clone(tool)))
            .collect();
        let owner_restricted_by_name = self
            .owner_restricted_by_name
            .iter()
            .filter(|(tool_name, _)| tool_name.as_str() != name)
            .map(|(tool_name, restricted)| (tool_name.clone(), *restricted))
            .collect();
        Self {
            ordered,
            by_name,
            owner_restricted_by_name,
        }
    }
}

pub(crate) fn default_factory() -> ToolFactory {
    let mut factory = ToolFactory::new();
    for tool in builtin::builtin_tools() {
        factory.register_builtin(tool);
    }
    factory
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Completed,
    Killed,
}

impl ProcessStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    pub session_id: String,
    pub command: String,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct ProcessManager {
    sessions: Mutex<HashMap<String, ProcessEntry>>,
    counter: AtomicU64,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    pub async fn spawn(
        &self,
        command: &str,
        workspace_root: Option<&std::path::Path>,
    ) -> Result<(String, CompletionHandle), FrameworkError> {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("proc-{}-{seq}", Utc::now().timestamp_millis());
        let base = std::env::temp_dir().join("simpleclaw_process");
        std::fs::create_dir_all(&base)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create temp dir: {e}")))?;
        let stdout_path = base.join(format!("{session_id}.stdout.log"));
        let stderr_path = base.join(format!("{session_id}.stderr.log"));
        let stdout_file = File::create(&stdout_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stdout log: {e}")))?;
        let stderr_file = File::create(&stderr_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stderr log: {e}")))?;

        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-lc").arg(command);
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
        let entry = ProcessEntry {
            command: command.to_owned(),
            status: ProcessStatus::Running,
            started_at,
            finished_at: None,
            pid,
            stdout_path,
            stderr_path,
            exit_code: None,
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.clone(), entry);
        Self::auto_evict(&mut sessions);
        drop(sessions);
        debug!(status = "started", "process session");
        Ok((session_id, handle))
    }

    fn auto_evict(sessions: &mut HashMap<String, ProcessEntry>) {
        const MAX_COMPLETED: usize = 64;
        let completed_count = sessions
            .values()
            .filter(|e| e.status != ProcessStatus::Running)
            .count();
        if completed_count <= MAX_COMPLETED {
            return;
        }
        let mut completed: Vec<(String, DateTime<Utc>)> = sessions
            .iter()
            .filter(|(_, e)| e.status != ProcessStatus::Running)
            .map(|(id, e)| (id.clone(), e.finished_at.unwrap_or(e.started_at)))
            .collect();
        completed.sort_by_key(|(_, t)| *t);
        let to_evict = completed_count - MAX_COMPLETED;
        for (id, _) in completed.into_iter().take(to_evict) {
            if let Some(entry) = sessions.remove(&id) {
                entry.cleanup_files();
            }
        }
    }

    pub async fn spawn_sandboxed(
        &self,
        command: &str,
        workspace_root: &std::path::Path,
        sandbox: &ToolSandboxConfig,
    ) -> Result<(String, CompletionHandle), FrameworkError> {
        let wrapped =
            sandbox_runtime::wrap_command_for_exec(command, workspace_root, sandbox).await?;

        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("proc-{}-{seq}", Utc::now().timestamp_millis());
        let base = std::env::temp_dir().join("simpleclaw_process");
        std::fs::create_dir_all(&base)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create temp dir: {e}")))?;
        let stdout_path = base.join(format!("{session_id}.stdout.log"));
        let stderr_path = base.join(format!("{session_id}.stderr.log"));
        let stdout_file = File::create(&stdout_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stdout log: {e}")))?;
        let stderr_file = File::create(&stderr_path)
            .map_err(|e| FrameworkError::Tool(format!("exec failed to create stderr log: {e}")))?;
        let workspace = sandbox::normalize_workspace_root(workspace_root)?;
        let child = std::process::Command::new("bash")
            .arg("-lc")
            .arg(wrapped)
            .current_dir(&workspace)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .map_err(|e| {
                FrameworkError::Tool(format!("exec failed to start sandbox runtime: {e}"))
            })?;

        let started_at = Utc::now();
        let pid = Some(child.id());
        let handle = CompletionHandle::Host(child);
        let entry = ProcessEntry {
            command: command.to_owned(),
            status: ProcessStatus::Running,
            started_at,
            finished_at: None,
            pid,
            stdout_path,
            stderr_path,
            exit_code: None,
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.clone(), entry);
        Self::auto_evict(&mut sessions);
        drop(sessions);
        debug!(status = "started", "process session");
        Ok((session_id, handle))
    }

    pub async fn update(&self, session_id: &str) -> Result<ProcessSnapshot, FrameworkError> {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.get_mut(session_id).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown process session_id: {session_id}"))
        })?;
        entry.poll_completion();
        Ok(entry.snapshot(session_id.to_owned()))
    }

    pub async fn list(&self) -> Vec<ProcessSnapshot> {
        // Phase 1: poll and collect metadata under lock.
        let metadata: Vec<ProcessEntryMeta> = {
            let mut sessions = self.sessions.lock().await;
            for entry in sessions.values_mut() {
                entry.poll_completion();
            }
            sessions
                .iter()
                .map(|(id, entry)| entry.metadata(id.clone()))
                .collect()
        };
        // Phase 2: read output files without holding the lock.
        let mut items: Vec<ProcessSnapshot> = metadata
            .into_iter()
            .map(|meta| {
                let stdout = read_process_output_tail(&meta.stdout_path, 32_768);
                let stderr = read_process_output_tail(&meta.stderr_path, 16_384);
                ProcessSnapshot {
                    session_id: meta.session_id,
                    command: meta.command,
                    status: meta.status,
                    pid: meta.pid,
                    started_at: meta.started_at,
                    finished_at: meta.finished_at,
                    exit_code: meta.exit_code,
                    stdout,
                    stderr,
                }
            })
            .collect();
        items.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        items
    }

    pub async fn kill(&self, session_id: &str) -> Result<ProcessSnapshot, FrameworkError> {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.get_mut(session_id).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown process session_id: {session_id}"))
        })?;
        entry.kill().await?;
        Ok(entry.snapshot(session_id.to_owned()))
    }

    pub fn spawn_completion_watcher(
        self: &Arc<Self>,
        session_id: String,
        handle: CompletionHandle,
        completion_tx: mpsc::Sender<InboundMessage>,
        route: CompletionRoute,
    ) {
        let pm = Arc::clone(self);
        let trace_id = route.trace_id.clone();
        let span = info_span!(
            "process.completion_watcher",
            trace_id = %trace_id,
            session_id = %route.session_key
        );
        tokio::spawn(
            async move {
                let exit_code = match handle {
                    CompletionHandle::Host(mut child) => {
                        let status = tokio::task::spawn_blocking(move || child.wait())
                            .await
                            .ok()
                            .and_then(|r| r.ok());
                        status.and_then(|s| s.code())
                    }
                };

                pm.mark_completed(&session_id, exit_code).await;

                let snapshot = pm.update(&session_id).await;
                let command = snapshot
                    .as_ref()
                    .map(|s| s.command.as_str())
                    .unwrap_or("unknown");
                let code = exit_code.unwrap_or(-1);
                let content = format!(
                    "[background process completed] session_id={} exit_code={} command={}",
                    session_id, code, command
                );
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
                if let Err(err) = completion_tx.send(msg).await {
                    tracing::warn!(
                        status = "failed",
                        error_kind = "completion_send",
                        error = %err,
                        "failed to send background process completion message"
                    );
                } else {
                    debug!(status = "completed", "process completion watcher");
                }
            }
            .instrument(span),
        );
    }

    async fn mark_completed(&self, session_id: &str, exit_code: Option<i32>) {
        let mut sessions = self.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(session_id)
            && entry.status == ProcessStatus::Running
        {
            entry.status = ProcessStatus::Completed;
            entry.exit_code = exit_code;
            entry.finished_at = Some(Utc::now());
        }
    }

    pub async fn forget(&self, session_id: &str) -> Result<ProcessSnapshot, FrameworkError> {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.get(session_id).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown process session_id: {session_id}"))
        })?;
        if entry.status == ProcessStatus::Running {
            return Err(FrameworkError::Tool(
                "cannot forget a running process — kill it first".to_owned(),
            ));
        }
        let entry = sessions.remove(session_id).unwrap();
        let snapshot = entry.snapshot(session_id.to_owned());
        entry.cleanup_files();
        Ok(snapshot)
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle passed to the completion watcher for event-driven (non-polling) wait.
pub enum CompletionHandle {
    Host(std::process::Child),
}

#[derive(Debug)]
struct ProcessEntry {
    command: String,
    status: ProcessStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    pid: Option<u32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    exit_code: Option<i32>,
}

struct ProcessEntryMeta {
    session_id: String,
    command: String,
    status: ProcessStatus,
    pid: Option<u32>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl ProcessEntry {
    fn poll_completion(&mut self) {
        // Host processes are tracked by the completion watcher which owns the Child.
        // No on-demand poll is possible here.
    }

    async fn kill(&mut self) -> Result<(), FrameworkError> {
        if self.status != ProcessStatus::Running {
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
        self.status = ProcessStatus::Killed;
        self.finished_at = Some(Utc::now());
        self.exit_code = Some(-1);
        Ok(())
    }

    fn metadata(&self, session_id: String) -> ProcessEntryMeta {
        ProcessEntryMeta {
            session_id,
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

    fn snapshot(&self, session_id: String) -> ProcessSnapshot {
        ProcessSnapshot {
            session_id,
            command: self.command.clone(),
            status: self.status.clone(),
            pid: self.pid,
            started_at: self.started_at,
            finished_at: self.finished_at,
            exit_code: self.exit_code,
            stdout: read_process_output_tail(&self.stdout_path, 32_768),
            stderr: read_process_output_tail(&self.stderr_path, 16_384),
        }
    }

    /// Delete log files.
    fn cleanup_files(self) {
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

impl Drop for ProcessManager {
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
    use super::*;
    #[derive(Clone)]
    struct FakeTool {
        desc: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &'static str {
            "process"
        }

        fn description(&self) -> &'static str {
            self.desc
        }

        fn input_schema_json(&self) -> &'static str {
            "{\"type\":\"null\"}"
        }

        async fn execute(
            &self,
            _ctx: &ToolExecEnv,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<String, FrameworkError> {
            Ok("ok".to_owned())
        }
    }

    #[derive(Clone)]
    struct NamedTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "named"
        }

        fn input_schema_json(&self) -> &'static str {
            "{\"type\":\"null\"}"
        }

        async fn execute(
            &self,
            _ctx: &ToolExecEnv,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<String, FrameworkError> {
            Ok("ok".to_owned())
        }
    }

    fn only_process_enabled() -> ToolsConfig {
        ToolsConfig {
            read: Some(crate::config::ReadToolConfig {
                enabled: false,
                ..Default::default()
            }),
            edit: Some(crate::config::EditToolConfig {
                enabled: false,
                ..Default::default()
            }),
            exec: Some(crate::config::ExecToolConfig {
                enabled: false,
                ..Default::default()
            }),
            process: Some(crate::config::ProcessToolConfig {
                enabled: true,
                ..Default::default()
            }),
            web_search: Some(crate::config::WebSearchToolConfig {
                enabled: false,
                ..Default::default()
            }),
            web_fetch: Some(crate::config::WebFetchToolConfig {
                enabled: false,
                ..Default::default()
            }),
            memory: Some(crate::config::MemoryToolConfig {
                enabled: false,
                ..Default::default()
            }),
            memorize: Some(crate::config::MemorizeToolConfig {
                enabled: false,
                ..Default::default()
            }),
            forget: Some(crate::config::ForgetToolConfig {
                enabled: false,
                ..Default::default()
            }),
            summon: Some(crate::config::SummonToolConfig {
                enabled: false,
                ..Default::default()
            }),
            task: Some(crate::config::TaskToolConfig {
                enabled: false,
                ..Default::default()
            }),
            clock: Some(crate::config::ClockToolConfig {
                enabled: false,
                ..Default::default()
            }),
            react: Some(crate::config::ReactToolConfig {
                enabled: false,
                ..Default::default()
            }),
            skills: Some(crate::config::SkillsToolConfig {
                enabled: false,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn register_overwrites_existing_tool_by_name() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(Box::new(FakeTool { desc: "fake-a" }));
        factory.register_builtin(Box::new(FakeTool { desc: "fake-b" }));

        let active = factory
            .resolve_active(&only_process_enabled(), &[])
            .expect("fake tool should resolve");
        let definitions = active.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].description, "fake-b");
    }

    #[test]
    fn tool_ctx_owner_allowed_when_owner_ids_empty_is_false() {
        assert!(!ToolExecEnv::owner_allowed("user-1", &[]));
    }

    #[test]
    fn tool_ctx_owner_allowed_when_user_matches() {
        let owner_ids = vec!["owner-1".to_owned(), "owner-2".to_owned()];
        assert!(ToolExecEnv::owner_allowed("owner-2", &owner_ids));
    }

    #[test]
    fn tool_ctx_owner_allowed_when_user_missing() {
        let owner_ids = vec!["owner-1".to_owned(), "owner-2".to_owned()];
        assert!(!ToolExecEnv::owner_allowed("user-3", &owner_ids));
    }

    #[test]
    fn resolve_active_uses_owner_restricted_flag_from_config() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(Box::new(NamedTool { name: "clock" }));
        let mut config = only_process_enabled();
        config.process = Some(crate::config::ProcessToolConfig {
            enabled: false,
            ..Default::default()
        });
        config.clock = Some(crate::config::ClockToolConfig {
            enabled: true,
            owner_restricted: false,
        });

        let active = factory.resolve_active(&config, &[]).expect("resolve tools");
        assert!(!active.owner_restricted("clock"));
    }

    #[test]
    fn resolve_active_marks_skill_tools_unrestricted() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(Box::new(NamedTool { name: "clock" }));
        let mut config = only_process_enabled();
        config.process = Some(crate::config::ProcessToolConfig {
            enabled: false,
            ..Default::default()
        });
        config.clock = Some(crate::config::ClockToolConfig {
            enabled: true,
            owner_restricted: true,
        });
        let skill_tool: Arc<dyn Tool> = Arc::new(NamedTool { name: "skill_demo" });

        let active = factory
            .resolve_active(&config, &[skill_tool])
            .expect("resolve tools");
        assert!(active.owner_restricted("clock"));
        assert!(!active.owner_restricted("skill_demo"));
    }

    #[test]
    fn active_tools_without_removes_target_tool() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(Box::new(NamedTool { name: "clock" }));
        factory.register_builtin(Box::new(NamedTool { name: "react" }));
        let mut config = only_process_enabled();
        config.process = Some(crate::config::ProcessToolConfig {
            enabled: false,
            ..Default::default()
        });
        config.clock = Some(crate::config::ClockToolConfig {
            enabled: true,
            owner_restricted: true,
        });
        config.react = Some(crate::config::ReactToolConfig {
            enabled: true,
            owner_restricted: false,
        });
        let active = factory.resolve_active(&config, &[]).expect("resolve tools");
        let filtered = active.without("react");
        assert!(active.get("react").is_some());
        assert!(filtered.get("react").is_none());
        assert!(filtered.get("clock").is_some());
    }
}
