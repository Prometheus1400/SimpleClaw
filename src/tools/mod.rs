mod async_tool_manager;
pub mod builtin;
pub mod skill;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::channels::InboundMessage;
use crate::config::{GatewayChannelKind, ToolsConfig};
use crate::dispatch::ToolExecutionResult;
use crate::error::FrameworkError;
use crate::gateway::Gateway;
use crate::memory::DynMemory;
use crate::providers::ToolDefinition;
use crate::sandbox::{DefaultHostSandbox, DefaultWasmSandbox, HostSandbox, WasmSandbox};

pub(crate) use async_tool_manager::StartedAsyncToolRun;
pub use async_tool_manager::{
    AsyncToolRunDetails, AsyncToolRunManager, AsyncToolRunSnapshot, AsyncToolRunStatus,
};

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
    pub allow_async_tools: bool,
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
    WebSearch(Arc<builtin::web_search::WebSearchTool>),
    WebFetch(Arc<builtin::web_fetch::WebFetchTool>),
    Read(Arc<builtin::read::ReadTool>),
    Edit(Arc<builtin::edit::EditTool>),
    Glob(Arc<builtin::glob::GlobTool>),
    Grep(Arc<builtin::grep::GrepTool>),
    List(Arc<builtin::list::ListTool>),
    Exec(Arc<builtin::exec::ExecTool>),
    Summon(Arc<builtin::summon::SummonTool>),
    Task(Arc<builtin::task::TaskTool>),
    Direct(Arc<dyn Tool>),
}

impl RegisteredTool {
    fn name(&self) -> &str {
        match self {
            Self::WebSearch(tool) => tool.name(),
            Self::WebFetch(tool) => tool.name(),
            Self::Read(tool) => tool.name(),
            Self::Edit(tool) => tool.name(),
            Self::Glob(tool) => tool.name(),
            Self::Grep(tool) => tool.name(),
            Self::List(tool) => tool.name(),
            Self::Exec(tool) => tool.name(),
            Self::Summon(tool) => tool.name(),
            Self::Task(tool) => tool.name(),
            Self::Direct(tool) => tool.name(),
        }
    }

    fn metadata(&self) -> ToolMetadata<'_> {
        match self {
            Self::WebSearch(tool) => tool.metadata(),
            Self::WebFetch(tool) => tool.metadata(),
            Self::Read(tool) => tool.metadata(),
            Self::Edit(tool) => tool.metadata(),
            Self::Glob(tool) => tool.metadata(),
            Self::Grep(tool) => tool.metadata(),
            Self::List(tool) => tool.metadata(),
            Self::Exec(tool) => tool.metadata(),
            Self::Summon(tool) => tool.metadata(),
            Self::Task(tool) => tool.metadata(),
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
            Self::WebSearch(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::WebSearch(Arc::new(next)))
            }
            Self::WebFetch(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::WebFetch(Arc::new(next)))
            }
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
            Self::Glob(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Glob(Arc::new(next)))
            }
            Self::Grep(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Grep(Arc::new(next)))
            }
            Self::List(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::List(Arc::new(next)))
            }
            Self::Exec(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Exec(Arc::new(next)))
            }
            Self::Summon(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Summon(Arc::new(next)))
            }
            Self::Task(tool) => {
                let mut next = (**tool).clone();
                if let Some(config) = config {
                    next.configure(config)?;
                }
                Ok(Self::Task(Arc::new(next)))
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
            Self::WebSearch(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::WebFetch(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Read(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Edit(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Glob(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Grep(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::List(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Exec(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Summon(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
            Self::Task(tool) => tool.execute_with_trace(ctx, args_json, session_id).await,
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

    pub(crate) fn with_async_disabled(&self) -> Self {
        let mut ordered = Vec::with_capacity(self.ordered.len());
        for entry in &self.ordered {
            let tool = match &*entry.tool {
                RegisteredTool::Exec(exec) => {
                    let mut next = (**exec).clone();
                    next.set_allow_background(false);
                    Arc::new(RegisteredTool::Exec(Arc::new(next)))
                }
                RegisteredTool::Summon(summon) => {
                    let mut next = (**summon).clone();
                    next.set_allow_background(false);
                    Arc::new(RegisteredTool::Summon(Arc::new(next)))
                }
                RegisteredTool::Task(task) => {
                    let mut next = (**task).clone();
                    next.set_allow_background(false);
                    Arc::new(RegisteredTool::Task(Arc::new(next)))
                }
                _ => Arc::clone(&entry.tool),
            };
            ordered.push(AgentToolEntry {
                tool,
                execution_kind: entry.execution_kind,
                owner_restricted: entry.owner_restricted,
            });
        }
        let by_name = ordered
            .iter()
            .map(|entry| (entry.tool.name().to_owned(), entry.clone()))
            .collect();
        Self { ordered, by_name }
    }

    pub(crate) fn with_async_disabled_if(&self, disable_async: bool) -> Self {
        if disable_async {
            self.with_async_disabled()
        } else {
            self.clone()
        }
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
        "glob" => {
            if tools_config
                .glob
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
        "grep" => {
            if tools_config
                .grep
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
        "list" => {
            if tools_config
                .list
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
        "web_search" => {
            if tools_config
                .web_search
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
        "web_fetch" => {
            if tools_config
                .web_fetch
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
            (RegisteredTool::WebSearch(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(args_json)?;
                tool.execute_direct(plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::WebSearch(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::WebFetch(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(args_json)?;
                tool.execute_direct(plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::WebFetch(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
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
            (RegisteredTool::Glob(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Glob(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Grep(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Grep(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::List(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::List(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_wasm(ctx, plan, self.wasm_runtime.as_ref())
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan, session_id).await
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::HostSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_host_sandboxed(
                    ctx,
                    plan,
                    session_id,
                    self.host_sandbox_runtime.as_ref(),
                )
                .await
            }
            (RegisteredTool::Summon(tool), ToolExecutionKind::Direct) => {
                tool.execute_with_trace(ctx, args_json, session_id).await
            }
            (RegisteredTool::Task(tool), ToolExecutionKind::Direct) => {
                tool.execute_with_trace(ctx, args_json, session_id).await
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
            "background"
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

    fn only_background_enabled() -> ToolsConfig {
        ToolsConfig {
            read: Some(crate::config::ReadToolConfig {
                enabled: false,
                ..Default::default()
            }),
            edit: Some(crate::config::EditToolConfig {
                enabled: false,
                ..Default::default()
            }),
            glob: Some(crate::config::GlobToolConfig {
                enabled: false,
                ..Default::default()
            }),
            grep: Some(crate::config::GrepToolConfig {
                enabled: false,
                ..Default::default()
            }),
            list: Some(crate::config::ListToolConfig {
                enabled: false,
                ..Default::default()
            }),
            exec: Some(crate::config::ExecToolConfig {
                enabled: false,
                ..Default::default()
            }),
            background: Some(crate::config::BackgroundToolConfig {
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
            .build_registry(&only_background_enabled(), &[])
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
        let mut config = only_background_enabled();
        config.background = Some(crate::config::BackgroundToolConfig {
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
        let mut config = only_background_enabled();
        config.background = Some(crate::config::BackgroundToolConfig {
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
        let mut config = only_background_enabled();
        config.background = Some(crate::config::BackgroundToolConfig {
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

    #[test]
    fn async_tool_manager_module_owns_async_run_api() {
        let _manager =
            std::any::TypeId::of::<crate::tools::async_tool_manager::AsyncToolRunManager>();
        let _status = crate::tools::async_tool_manager::AsyncToolRunStatus::Running;
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

    #[test]
    fn exec_tool_definition_hides_background_when_disabled() {
        let factory = default_factory();
        let mut config = only_background_enabled();
        config.background = Some(crate::config::BackgroundToolConfig {
            enabled: false,
            ..Default::default()
        });
        config.exec = Some(crate::config::ExecToolConfig {
            enabled: true,
            allow_background: false,
            ..Default::default()
        });

        let active = factory
            .build_registry(&config, &[])
            .expect("tool registry should build");
        let exec = active
            .definitions()
            .into_iter()
            .find(|tool| tool.name == "exec")
            .expect("exec definition should exist");

        assert!(!exec.input_schema_json.contains("background"));
        assert!(!exec.description.contains("background"));
    }
}
