pub mod builtin;
pub mod skill;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
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
use crate::config::{GatewayChannelKind, ToolsConfig};
use crate::dispatch::ToolExecutionResult;
use crate::error::FrameworkError;
use crate::gateway::Gateway;
use crate::memory::DynMemory;
use crate::providers::ToolDefinition;
use crate::sandbox::{DefaultHostSandbox, DefaultWasmSandbox, HostSandbox, WasmSandbox};

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
pub(crate) enum ToolExecutionOutcome {
    Completed(ToolRunOutput),
    AsyncStarted(StartedAsyncToolRun),
}

impl ToolExecutionOutcome {
    pub(crate) fn completed(output: String) -> Self {
        Self::Completed(ToolRunOutput::plain(output))
    }
}

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
    pub history_messages: usize,
    pub env: BTreeMap<String, String>,
    pub persona_root: PathBuf,
    pub workspace_root: PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub async_tool_runs: Arc<AsyncToolRunManager>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionKind {
    Direct,
    WasmSandbox,
    HostSandbox,
}

#[derive(Debug, Clone)]
pub struct ToolMetadata<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub input_schema_json: &'a str,
    pub supported_execution_kinds: &'static [ToolExecutionKind],
}

#[async_trait]
pub(crate) trait Tool: Send + Sync + ToolClone {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema_json(&self) -> &str;
    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct]
    }
    fn metadata(&self) -> ToolMetadata<'_> {
        ToolMetadata {
            name: self.name(),
            description: self.description(),
            input_schema_json: self.input_schema_json(),
            supported_execution_kinds: self.supported_execution_kinds(),
        }
    }
    fn configure(&mut self, _config: Value) -> Result<(), FrameworkError> {
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError>;

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        self.execute(ctx, args_json, session_id).await
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

#[derive(Clone)]
pub(crate) enum RegisteredTool {
    Read(Arc<builtin::read::ReadTool>),
    Edit(Arc<builtin::edit::EditTool>),
    Exec(Arc<builtin::exec::ExecTool>),
    Direct(Arc<dyn Tool>),
}

impl RegisteredTool {
    fn name(&self) -> &str {
        match self {
            Self::Read(tool) => tool.name(),
            Self::Edit(tool) => tool.name(),
            Self::Exec(tool) => tool.name(),
            Self::Direct(tool) => tool.name(),
        }
    }

    fn metadata(&self) -> ToolMetadata<'_> {
        match self {
            Self::Read(tool) => tool.metadata(),
            Self::Edit(tool) => tool.metadata(),
            Self::Exec(tool) => tool.metadata(),
            Self::Direct(tool) => tool.metadata(),
        }
    }

    fn definition(&self) -> ToolDefinition {
        let metadata = self.metadata();
        ToolDefinition {
            name: metadata.name.to_owned(),
            description: metadata.description.to_owned(),
            input_schema_json: metadata.input_schema_json.to_owned(),
        }
    }

    fn configure_clone(&self, config: Option<Value>) -> Result<Self, FrameworkError> {
        match self {
            Self::Read(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Read(Arc::new(next)))
            }
            Self::Edit(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Edit(Arc::new(next)))
            }
            Self::Exec(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Exec(Arc::new(next)))
            }
            Self::Direct(tool) => {
                let mut next = tool.box_clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Direct(Arc::from(next)))
            }
        }
    }

    fn supports_execution_kind(&self, kind: ToolExecutionKind) -> bool {
        self.metadata().supported_execution_kinds.contains(&kind)
    }

    async fn execute_direct(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        match self {
            Self::Read(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Edit(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Exec(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Direct(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentToolEntry {
    pub tool: Arc<RegisteredTool>,
    pub execution_kind: ToolExecutionKind,
    pub owner_restricted: bool,
}

#[derive(Clone)]
pub(crate) struct AgentToolRegistry {
    ordered: Vec<AgentToolEntry>,
    by_name: HashMap<String, AgentToolEntry>,
}

impl std::fmt::Debug for AgentToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries: Vec<_> = self
            .ordered
            .iter()
            .map(|entry| {
                (
                    entry.tool.name().to_owned(),
                    entry.execution_kind,
                    entry.owner_restricted,
                )
            })
            .collect();
        f.debug_struct("AgentToolRegistry")
            .field("entries", &entries)
            .finish()
    }
}

impl AgentToolRegistry {
    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.ordered
            .iter()
            .map(|entry| entry.tool.definition())
            .collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&AgentToolEntry> {
        self.by_name.get(name)
    }

    pub(crate) fn without(&self, name: &str) -> Self {
        let ordered: Vec<AgentToolEntry> = self
            .ordered
            .iter()
            .filter(|entry| entry.tool.name() != name)
            .cloned()
            .collect();
        let by_name = ordered
            .iter()
            .map(|entry| (entry.tool.name().to_owned(), entry.clone()))
            .collect();
        Self { ordered, by_name }
    }

    pub(crate) fn without_names(&self, names: &[&str]) -> Self {
        names
            .iter()
            .fold(self.clone(), |registry, name| registry.without(name))
    }
}

#[derive(Default)]
pub(crate) struct ToolFactory {
    builtins: Vec<RegisteredTool>,
    by_name: HashMap<String, RegisteredTool>,
}

impl ToolFactory {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register_builtin(&mut self, tool: RegisteredTool) {
        let name = tool.name().to_owned();
        if let Some(existing_idx) = self
            .builtins
            .iter()
            .position(|candidate| candidate.name() == name)
        {
            self.builtins[existing_idx] = tool.clone();
        } else {
            self.builtins.push(tool.clone());
        }
        self.by_name.insert(name, tool);
    }

    pub(crate) fn build_registry(
        &self,
        tools_config: &ToolsConfig,
        skill_tools: &[Arc<dyn Tool>],
    ) -> Result<AgentToolRegistry, FrameworkError> {
        let mut ordered = Vec::new();
        let mut by_name = HashMap::new();
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
            let tool =
                Arc::new(tool_template.configure_clone(tools_config.config_for_tool(&name)?)?);
            let owner_restricted = tools_config
                .owner_restricted_for_tool(&name)
                .unwrap_or(true);
            let execution_kind = select_execution_kind(&name, tools_config)?;
            if !tool.supports_execution_kind(execution_kind) {
                return Err(FrameworkError::Config(format!(
                    "tool '{name}' does not support execution kind {:?}",
                    execution_kind
                )));
            }
            let entry = AgentToolEntry {
                tool,
                execution_kind,
                owner_restricted,
            };
            ordered.push(entry.clone());
            by_name.insert(name, entry);
        }

        for tool in skill_tools {
            let name = tool.name().to_owned();
            if !seen.insert(name.clone()) {
                continue;
            }
            let entry = AgentToolEntry {
                tool: Arc::new(RegisteredTool::Direct(Arc::clone(tool))),
                execution_kind: ToolExecutionKind::Direct,
                owner_restricted: false,
            };
            ordered.push(entry.clone());
            by_name.insert(name, entry);
        }

        Ok(AgentToolRegistry { ordered, by_name })
    }
}

pub(crate) fn default_factory() -> ToolFactory {
    let mut factory = ToolFactory::new();
    for tool in builtin::builtin_tools() {
        factory.register_builtin(tool);
    }
    factory
}

fn select_execution_kind(
    name: &str,
    tools_config: &ToolsConfig,
) -> Result<ToolExecutionKind, FrameworkError> {
    let kind = match name {
        "read" => {
            if tools_config
                .read
                .clone()
                .unwrap_or_default()
                .sandbox
                .enabled
            {
                ToolExecutionKind::WasmSandbox
            } else {
                ToolExecutionKind::Direct
            }
        }
        "edit" => {
            if tools_config
                .edit
                .clone()
                .unwrap_or_default()
                .sandbox
                .enabled
            {
                ToolExecutionKind::WasmSandbox
            } else {
                ToolExecutionKind::Direct
            }
        }
        "exec" => {
            if tools_config
                .exec
                .clone()
                .unwrap_or_default()
                .sandbox
                .enabled
            {
                ToolExecutionKind::HostSandbox
            } else {
                ToolExecutionKind::Direct
            }
        }
        _ => ToolExecutionKind::Direct,
    };
    Ok(kind)
}

pub(crate) trait ToolAuthorizer: Send + Sync {
    fn authorize(&self, entry: &AgentToolEntry, ctx: &ToolExecEnv) -> Result<(), FrameworkError>;
}

pub(crate) struct DefaultToolAuthorizer;

impl ToolAuthorizer for DefaultToolAuthorizer {
    fn authorize(&self, entry: &AgentToolEntry, ctx: &ToolExecEnv) -> Result<(), FrameworkError> {
        if !entry.owner_restricted {
            return Ok(());
        }
        if ctx.owner_ids.is_empty() {
            return Err(FrameworkError::Tool(
                "owner restriction misconfigured: runtime.owner_ids is empty".to_owned(),
            ));
        }
        if ctx.is_owner() {
            return Ok(());
        }
        Err(FrameworkError::Tool(
            "permission denied: this tool is restricted to the owner".to_owned(),
        ))
    }
}

#[async_trait]
pub(crate) trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError>;
}

pub(crate) struct DefaultToolExecutor {
    wasm_runtime: Arc<dyn WasmSandbox>,
    host_sandbox_runtime: Arc<dyn HostSandbox>,
}

impl DefaultToolExecutor {
    pub(crate) fn new() -> Self {
        Self {
            wasm_runtime: Arc::new(DefaultWasmSandbox),
            host_sandbox_runtime: Arc::new(DefaultHostSandbox),
        }
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        match (&*entry.tool, entry.execution_kind) {
            (RegisteredTool::Read(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Read(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Edit(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Edit(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan).await
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::HostSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_host_sandboxed(ctx, plan, self.host_sandbox_runtime.as_ref())
                    .await
            }
            (tool, ToolExecutionKind::Direct) => {
                tool.execute_direct(ctx, args_json, session_id).await
            }
            (tool, kind) => Err(FrameworkError::Tool(format!(
                "unsupported execution kind {:?} for tool '{}'",
                kind,
                tool.name()
            ))),
        }
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
        prepared: crate::sandbox::PreparedHostCommand,
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

    pub fn spawn_completion_watcher(
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

/// Handle passed to the completion watcher for event-driven (non-polling) wait.
pub enum CompletionHandle {
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

    /// Delete log files.
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
    use super::*;
    use tokio::time::{Duration, sleep};
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
        ) -> Result<ToolExecutionOutcome, FrameworkError> {
            Ok(ToolExecutionOutcome::completed("ok".to_owned()))
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
        ) -> Result<ToolExecutionOutcome, FrameworkError> {
            Ok(ToolExecutionOutcome::completed("ok".to_owned()))
        }
    }

    #[derive(Clone)]
    struct OwnedMetadataTool {
        name: String,
        description: String,
        input_schema_json: String,
    }

    #[async_trait]
    impl Tool for OwnedMetadataTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn input_schema_json(&self) -> &str {
            &self.input_schema_json
        }

        async fn execute(
            &self,
            _ctx: &ToolExecEnv,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<ToolExecutionOutcome, FrameworkError> {
            Ok(ToolExecutionOutcome::completed("ok".to_owned()))
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
            cron: Some(crate::config::CronToolConfig {
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
        factory.register_builtin(RegisteredTool::Direct(Arc::new(FakeTool {
            desc: "fake-a",
        })));
        factory.register_builtin(RegisteredTool::Direct(Arc::new(FakeTool {
            desc: "fake-b",
        })));

        let active = factory
            .build_registry(&only_process_enabled(), &[])
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
        factory.register_builtin(RegisteredTool::Direct(Arc::new(NamedTool {
            name: "clock",
        })));
        let mut config = only_process_enabled();
        config.process = Some(crate::config::ProcessToolConfig {
            enabled: false,
            ..Default::default()
        });
        config.clock = Some(crate::config::ClockToolConfig {
            enabled: true,
            owner_restricted: false,
        });

        let active = factory.build_registry(&config, &[]).expect("resolve tools");
        assert!(!active.get("clock").expect("clock entry").owner_restricted);
    }

    #[test]
    fn resolve_active_marks_skill_tools_unrestricted() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(RegisteredTool::Direct(Arc::new(NamedTool {
            name: "clock",
        })));
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
            .build_registry(&config, &[skill_tool])
            .expect("resolve tools");
        assert!(active.get("clock").expect("clock entry").owner_restricted);
        assert!(
            !active
                .get("skill_demo")
                .expect("skill entry")
                .owner_restricted
        );
    }

    #[test]
    fn active_tools_without_removes_target_tool() {
        let mut factory = ToolFactory::new();
        factory.register_builtin(RegisteredTool::Direct(Arc::new(NamedTool {
            name: "clock",
        })));
        factory.register_builtin(RegisteredTool::Direct(Arc::new(NamedTool {
            name: "react",
        })));
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
        let active = factory.build_registry(&config, &[]).expect("resolve tools");
        let filtered = active.without("react");
        assert!(active.get("react").is_some());
        assert!(filtered.get("react").is_none());
        assert!(filtered.get("clock").is_some());
    }

    #[test]
    fn registered_tool_definition_supports_owned_metadata_fields() {
        let tool = RegisteredTool::Direct(Arc::new(OwnedMetadataTool {
            name: "dynamic_tool".to_owned(),
            description: "metadata from owned strings".to_owned(),
            input_schema_json: "{\"type\":\"object\"}".to_owned(),
        }));

        let metadata = tool.metadata();
        assert_eq!(metadata.name, "dynamic_tool");
        assert_eq!(metadata.description, "metadata from owned strings");
        assert_eq!(metadata.input_schema_json, "{\"type\":\"object\"}");

        let definition = tool.definition();
        assert_eq!(definition.name, "dynamic_tool");
        assert_eq!(definition.description, "metadata from owned strings");
        assert_eq!(definition.input_schema_json, "{\"type\":\"object\"}");
    }

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

    #[test]
    fn sandbox_module_exposes_neutral_api_types() {
        let request = crate::sandbox::RunHostCommandRequest {
            command: "echo hello".to_owned(),
            workspace_root: PathBuf::from("/tmp/work"),
            policy: crate::sandbox::SandboxPolicy::default(),
            env: BTreeMap::new(),
            timeout_seconds: 5,
        };

        assert_eq!(request.command, "echo hello");
        let _prepared = std::any::TypeId::of::<crate::sandbox::PreparedHostCommand>();
        let _wasm_result = crate::sandbox::WasmRunResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        };
    }
}
