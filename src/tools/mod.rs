mod async_tool_manager;
pub mod builtin;
pub mod skill;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::approval::{ApprovalDecision, ApprovalRequest, DynApprovalRequester};
use crate::channels::InboundMessage;
use crate::config::{GatewayChannelKind, ToolsConfig};
use crate::dispatch::ToolExecutionResult;
use crate::error::{ApprovalDenied, FrameworkError};
use crate::gateway::Gateway;
use crate::memory::Memory;
use crate::providers::ToolDefinition;
use crate::sandbox::{DefaultHostSandbox, DefaultWasmSandbox, HostSandbox, WasmSandbox};
use crate::tools::builtin::file_access::FileToolRoute;

pub(crate) use async_tool_manager::StartedAsyncToolRun;
pub use async_tool_manager::{
    AsyncToolRunDetails, AsyncToolRunManager, AsyncToolRunSnapshot, AsyncToolRunStatus,
};

#[derive(Clone)]
pub struct AgentInvokeRequest {
    pub target_agent_id: String,
    pub session_id: String,
    pub user_id: String,
    pub prompt: String,
    pub progress_log: Option<PathBuf>,
    pub approval_requester: DynApprovalRequester,
}

#[derive(Clone)]
pub struct WorkerInvokeRequest {
    pub current_agent_id: String,
    pub session_id: String,
    pub user_id: String,
    pub prompt: String,
    pub max_steps_override: Option<u32>,
    pub progress_log: Option<PathBuf>,
    pub approval_requester: DynApprovalRequester,
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

pub(crate) struct ToolExecEnv<'a> {
    pub agent_id: &'a str,
    pub agent_name: &'a str,
    pub memory: &'a dyn Memory,
    pub history_messages: usize,
    pub env: &'a BTreeMap<String, String>,
    pub persona_root: &'a Path,
    pub workspace_root: &'a Path,
    pub user_id: &'a str,
    pub owner_ids: &'a [String],
    pub async_tool_runs: &'a Arc<AsyncToolRunManager>,
    pub invoker: &'a Arc<dyn AgentInvoker>,
    pub gateway: Option<&'a Gateway>,
    pub completion_tx: Option<&'a mpsc::Sender<InboundMessage>>,
    pub completion_route: Option<&'a CompletionRoute>,
    pub allow_async_tools: bool,
    pub approval_requester: DynApprovalRequester,
}

impl ToolExecEnv<'_> {
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
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError>;

    async fn execute_with_trace(
        &self,
        ctx: &ToolExecEnv<'_>,
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
        ctx: &ToolExecEnv<'_>,
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
pub struct AgentToolRegistry {
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
    fn authorize(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv<'_>,
    ) -> Result<(), FrameworkError>;
}

pub(crate) struct DefaultToolAuthorizer;

impl ToolAuthorizer for DefaultToolAuthorizer {
    fn authorize(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv<'_>,
    ) -> Result<(), FrameworkError> {
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
        Err(FrameworkError::Tool(format!(
            "permission denied: tool '{}' is restricted to the persona owner (caller='{}'). Only the persona owner can invoke this tool.",
            entry.tool.name(),
            ctx.user_id,
        )))
    }
}

#[async_trait]
pub(crate) trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv<'_>,
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

    #[cfg(test)]
    fn with_runtimes(
        wasm_runtime: Arc<dyn WasmSandbox>,
        host_sandbox_runtime: Arc<dyn HostSandbox>,
    ) -> Self {
        Self {
            wasm_runtime,
            host_sandbox_runtime,
        }
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(
        &self,
        entry: &AgentToolEntry,
        ctx: &ToolExecEnv<'_>,
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
                match &plan.route {
                    FileToolRoute::Sandboxed { .. } => {
                        let output = tool
                            .execute_wasm(ctx, plan.clone(), self.wasm_runtime.as_ref())
                            .await
                            .map_err(|err| invariant_sandbox_denial("read", err))?;
                        Ok(ToolExecutionOutcome::Completed(output))
                    }
                    FileToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "read",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan)
                            .await
                            .map(ToolExecutionOutcome::Completed)
                    }
                }
            }
            (RegisteredTool::Edit(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Edit(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                match &plan.route {
                    FileToolRoute::Sandboxed { .. } => {
                        let output = tool
                            .execute_wasm(ctx, plan.clone(), self.wasm_runtime.as_ref())
                            .await
                            .map_err(|err| invariant_sandbox_denial("edit", err))?;
                        Ok(ToolExecutionOutcome::Completed(output))
                    }
                    FileToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "edit",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan)
                            .await
                            .map(ToolExecutionOutcome::Completed)
                    }
                }
            }
            (RegisteredTool::Glob(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Glob(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                match &plan.route {
                    FileToolRoute::Sandboxed { .. } => {
                        let output = tool
                            .execute_wasm(ctx, plan.clone(), self.wasm_runtime.as_ref())
                            .await
                            .map_err(|err| invariant_sandbox_denial("glob", err))?;
                        Ok(ToolExecutionOutcome::Completed(output))
                    }
                    FileToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "glob",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan)
                            .await
                            .map(ToolExecutionOutcome::Completed)
                    }
                }
            }
            (RegisteredTool::Grep(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::Grep(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                match &plan.route {
                    FileToolRoute::Sandboxed { .. } => {
                        let output = tool
                            .execute_wasm(ctx, plan.clone(), self.wasm_runtime.as_ref())
                            .await
                            .map_err(|err| invariant_sandbox_denial("grep", err))?;
                        Ok(ToolExecutionOutcome::Completed(output))
                    }
                    FileToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "grep",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan)
                            .await
                            .map(ToolExecutionOutcome::Completed)
                    }
                }
            }
            (RegisteredTool::List(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan)
                    .await
                    .map(ToolExecutionOutcome::Completed)
            }
            (RegisteredTool::List(tool), ToolExecutionKind::WasmSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                match &plan.route {
                    FileToolRoute::Sandboxed { .. } => {
                        let output = tool
                            .execute_wasm(ctx, plan.clone(), self.wasm_runtime.as_ref())
                            .await
                            .map_err(|err| invariant_sandbox_denial("list", err))?;
                        Ok(ToolExecutionOutcome::Completed(output))
                    }
                    FileToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "list",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan)
                            .await
                            .map(ToolExecutionOutcome::Completed)
                    }
                }
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::Direct) => {
                let plan = tool.plan(ctx, args_json)?;
                tool.execute_direct(ctx, plan, session_id).await
            }
            (RegisteredTool::Exec(tool), ToolExecutionKind::HostSandbox) => {
                let plan = tool.plan(ctx, args_json)?;
                match &plan.route {
                    crate::tools::builtin::exec::ExecToolRoute::Sandboxed => {
                        match tool
                            .execute_host_sandboxed(
                                ctx,
                                plan.clone(),
                                session_id,
                                self.host_sandbox_runtime.as_ref(),
                            )
                            .await
                        {
                            Ok(outcome) => Ok(outcome),
                            Err(err) => {
                                let denial = err.as_sandbox_permission_denied();
                                if let Some(denial) = denial {
                                    let reason = format!(
                                        "{} requires unsandboxed {} access",
                                        denial.tool_name,
                                        denial.capability.as_str()
                                    );
                                    request_tool_escalation(
                                        ctx,
                                        session_id,
                                        &denial.tool_name,
                                        denial.capability,
                                        denial.target,
                                        reason,
                                    )
                                    .await?;
                                    tool.execute_direct(ctx, plan, session_id).await
                                } else {
                                    Err(err)
                                }
                            }
                        }
                    }
                    crate::tools::builtin::exec::ExecToolRoute::NeedsApproval {
                        capability,
                        target,
                        reason,
                    } => {
                        request_tool_escalation(
                            ctx,
                            session_id,
                            "exec",
                            capability.clone(),
                            target.clone(),
                            reason.clone(),
                        )
                        .await?;
                        tool.execute_direct(ctx, plan, session_id).await
                    }
                }
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

async fn request_tool_escalation(
    ctx: &ToolExecEnv<'_>,
    session_id: &str,
    tool_name: &str,
    capability: crate::error::SandboxCapability,
    target: String,
    reason: String,
) -> Result<(), FrameworkError> {
    let request = ApprovalRequest {
        agent_id: ctx.agent_id.to_owned(),
        agent_name: ctx.agent_name.to_owned(),
        session_id: session_id.to_owned(),
        requesting_user_id: ctx.user_id.to_owned(),
        tool_name: tool_name.to_owned(),
        execution_kind: "preflight_escalation".to_owned(),
        capability,
        reason,
        action_summary: target.clone(),
        diagnostic: format!("preflight escalation required: tool={tool_name} target={target}"),
    };

    match ctx.approval_requester.request_approval(request).await? {
        ApprovalDecision::Approved => Ok(()),
        ApprovalDecision::Denied => Err(FrameworkError::approval_denied(ApprovalDenied {
            approval_id: "denied".to_owned(),
            tool_name: tool_name.to_owned(),
            reason: "user denied sandbox escalation".to_owned(),
        })),
        ApprovalDecision::TimedOut => Err(FrameworkError::approval_denied(ApprovalDenied {
            approval_id: "timed-out".to_owned(),
            tool_name: tool_name.to_owned(),
            reason: "approval timed out".to_owned(),
        })),
    }
}

fn invariant_sandbox_denial(tool_name: &str, err: FrameworkError) -> FrameworkError {
    if let Some(denial) = err.as_sandbox_permission_denied() {
        return FrameworkError::Tool(format!(
            "sandbox invariant violated for {tool_name}: preflight classified target as in-sandbox but wasm denied {} access to {} ({})",
            denial.capability.as_str(),
            denial.target,
            denial.diagnostic
        ));
    }
    err
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::sync::{Mutex, mpsc};

    use super::*;
    use crate::approval::{ApprovalDecision, ApprovalRegistry, GatewayApprovalRequester};
    use crate::channels::{Channel, ChannelInbound};
    use crate::config::{ChannelOutputMode, GatewayChannelKind, RoutingConfig};
    use crate::error::FrameworkError;
    use crate::memory::{
        LongTermFactSummary, LongTermForgetResult, MemorizeResult, Memory, MemoryRecallHit,
        MemoryStoreScope, StoredMessage, StoredRole,
    };
    use crate::sandbox::{
        HostRunResult, PreparedHostCommand, RunHostCommandRequest, RunWasmRequest,
        SpawnHostCommandRequest, WasmRunResult,
    };

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
            _ctx: &ToolExecEnv<'_>,
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
            _ctx: &ToolExecEnv<'_>,
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
            _ctx: &ToolExecEnv<'_>,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<ToolExecutionOutcome, FrameworkError> {
            Ok(ToolExecutionOutcome::completed("ok".to_owned()))
        }
    }

    #[derive(Default)]
    struct NoopMemory;

    #[async_trait]
    impl Memory for NoopMemory {
        async fn append_message(
            &self,
            _session_id: &str,
            _role: StoredRole,
            _content: &str,
            _username: Option<&str>,
        ) -> Result<(), FrameworkError> {
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
            _config: &crate::config::MemoryRecallConfig,
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

    struct NoopInvoker;

    #[async_trait]
    impl AgentInvoker for NoopInvoker {
        async fn invoke_agent(
            &self,
            _request: AgentInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: String::new(),
                tool_calls: Vec::new(),
            })
        }

        async fn invoke_worker(
            &self,
            _request: WorkerInvokeRequest,
        ) -> Result<InvokeOutcome, FrameworkError> {
            Ok(InvokeOutcome {
                reply: String::new(),
                tool_calls: Vec::new(),
            })
        }
    }

    #[derive(Default)]
    struct CaptureChannel {
        sent_messages: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Channel for CaptureChannel {
        async fn send_message(
            &self,
            _channel_id: &str,
            content: &str,
        ) -> Result<(), FrameworkError> {
            self.sent_messages.lock().await.push(content.to_owned());
            Ok(())
        }

        async fn send_message_with_id(
            &self,
            channel_id: &str,
            content: &str,
        ) -> Result<Option<String>, FrameworkError> {
            self.send_message(channel_id, content).await?;
            Ok(Some("approval-message".to_owned()))
        }

        async fn edit_message(
            &self,
            _channel_id: &str,
            _message_id: &str,
            _content: &str,
        ) -> Result<(), FrameworkError> {
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

        async fn broadcast_typing(&self, _channel_id: &str) -> Result<(), FrameworkError> {
            Ok(())
        }

        async fn listen(&self) -> Result<ChannelInbound, FrameworkError> {
            Err(FrameworkError::Tool(
                "listen should not be called in tool executor tests".to_owned(),
            ))
        }
    }

    struct StubWasmSandbox {
        calls: AtomicUsize,
        stderr: String,
    }

    #[async_trait]
    impl WasmSandbox for StubWasmSandbox {
        async fn run(&self, _request: RunWasmRequest) -> Result<WasmRunResult, FrameworkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(WasmRunResult {
                stdout: String::new(),
                stderr: self.stderr.clone(),
                exit_code: 1,
            })
        }
    }

    struct PanicHostSandbox;

    #[async_trait]
    impl HostSandbox for PanicHostSandbox {
        async fn run(
            &self,
            _request: RunHostCommandRequest,
        ) -> Result<HostRunResult, FrameworkError> {
            panic!("host sandbox should not be used in this test");
        }

        async fn prepare_spawn(
            &self,
            _request: SpawnHostCommandRequest,
        ) -> Result<PreparedHostCommand, FrameworkError> {
            panic!("host sandbox should not be used in this test");
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("simpleclaw_tools_{prefix}_{nanos}"));
        std::fs::create_dir_all(&path).expect("temp dir should be created");
        path
    }

    fn test_gateway(channel: Arc<dyn Channel>) -> Arc<Gateway> {
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel);
        Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        ))
    }

    fn test_tool_env(
        workspace_root: PathBuf,
        gateway: Arc<Gateway>,
        approval_registry: Arc<ApprovalRegistry>,
        env_map: BTreeMap<String, String>,
    ) -> ToolExecEnv<'static> {
        let (completion_tx, _completion_rx) = mpsc::channel(4);
        let inbound = InboundMessage {
            trace_id: "trace-1".to_owned(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "agent-1".to_owned(),
            session_key: "sess-1".to_owned(),
            source_message_id: Some("msg-1".to_owned()),
            channel_id: "chan-1".to_owned(),
            guild_id: None,
            is_dm: true,
            user_id: "system".to_owned(),
            username: "system".to_owned(),
            mentioned_bot: false,
            invoke: false,
            content: String::new(),
            kind: crate::channels::InboundMessageKind::Text,
        };
        let memory = Box::leak(Box::new(NoopMemory));
        let env = Box::leak(Box::new(env_map));
        let persona_root = Box::leak(Box::new(workspace_root.clone()));
        let workspace_root = Box::leak(Box::new(workspace_root));
        let owner_ids = Box::leak(Box::new(vec!["owner-1".to_owned()]));
        let async_tool_runs = Box::leak(Box::new(Arc::new(AsyncToolRunManager::new())));
        let invoker: &'static Arc<dyn AgentInvoker> =
            Box::leak(Box::new(Arc::new(NoopInvoker) as Arc<dyn AgentInvoker>));
        let gateway_ref: &'static Gateway = Box::leak(Box::new((*gateway).clone()));
        let completion_tx = Box::leak(Box::new(completion_tx));
        let completion_route = Box::leak(Box::new(CompletionRoute {
            trace_id: "trace-1".to_owned(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "agent-1".to_owned(),
            session_key: "sess-1".to_owned(),
            source_message_id: Some("msg-1".to_owned()),
            channel_id: "chan-1".to_owned(),
            guild_id: None,
            is_dm: true,
        }));
        ToolExecEnv {
            agent_id: "agent-1",
            agent_name: "Agent One",
            memory,
            history_messages: 8,
            env,
            persona_root,
            workspace_root,
            user_id: "owner-1",
            owner_ids,
            async_tool_runs,
            invoker,
            gateway: Some(gateway_ref),
            completion_tx: Some(completion_tx),
            completion_route: Some(completion_route),
            allow_async_tools: true,
            approval_requester: Arc::new(GatewayApprovalRequester::new(
                approval_registry,
                Arc::clone(&gateway),
                inbound,
                Duration::from_secs(30),
            )),
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

    fn only_read_enabled() -> ToolsConfig {
        ToolsConfig {
            read: Some(crate::config::ReadToolConfig::default()),
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
                enabled: false,
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

    fn only_edit_enabled() -> ToolsConfig {
        ToolsConfig {
            read: Some(crate::config::ReadToolConfig {
                enabled: false,
                ..Default::default()
            }),
            edit: Some(crate::config::EditToolConfig::default()),
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
                enabled: false,
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

    fn only_exec_enabled() -> ToolsConfig {
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
            exec: Some(crate::config::ExecToolConfig::default()),
            background: Some(crate::config::BackgroundToolConfig {
                enabled: false,
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

    #[tokio::test]
    async fn outside_sandbox_edit_requests_approval_and_skips_wasm() {
        let workspace = unique_temp_dir("approval_retry");
        let outside = unique_temp_dir("approval_retry_outside");
        std::fs::write(outside.join("notes.txt"), "hello\n").expect("test file should exist");
        let approvals = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(CaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let ctx = test_tool_env(workspace, gateway, Arc::clone(&approvals), BTreeMap::new());
        let registry = default_factory()
            .build_registry(&only_edit_enabled(), &[])
            .expect("tool registry should build");
        let entry = registry
            .get("edit")
            .expect("edit tool should exist")
            .clone();
        let wasm = Arc::new(StubWasmSandbox {
            calls: AtomicUsize::new(0),
            stderr: "path denied by sandbox".to_owned(),
        });
        let executor = DefaultToolExecutor::with_runtimes(wasm.clone(), Arc::new(PanicHostSandbox));

        let approvals_task = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if let Some(request) = approvals.pending_requests().await.into_iter().next() {
                        let _ = approvals
                            .resolve(
                                &request.approval_id,
                                &request.requesting_user_id,
                                ApprovalDecision::Approved,
                            )
                            .await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("approval request was never created");
            })
        };

        let outcome = executor
            .execute(
                &entry,
                &ctx,
                &format!(
                    r#"{{"filePath":"{}","oldString":"hello","newString":"updated"}}"#,
                    outside.join("notes.txt").display()
                ),
                "sess-1",
            )
            .await
            .expect("approval should allow direct retry");
        approvals_task.await.expect("approval task should finish");

        let ToolExecutionOutcome::Completed(output) = outcome else {
            panic!("expected completed tool output");
        };
        let sent = channel.sent_messages.lock().await.clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Approval required."));
        assert_eq!(wasm.calls.load(Ordering::SeqCst), 0);
        assert!(output.output.contains("Edit applied successfully"));
        let updated = std::fs::read_to_string(outside.join("notes.txt")).expect("edited file");
        assert_eq!(updated, "updated\n");
    }

    #[tokio::test]
    async fn outside_sandbox_read_returns_approval_denied_when_rejected() {
        let workspace = unique_temp_dir("approval_deny");
        let outside = unique_temp_dir("approval_deny_outside");
        std::fs::write(outside.join("notes.txt"), "hello\n").expect("test file should exist");
        let approvals = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(CaptureChannel::default());
        let gateway = test_gateway(channel);
        let ctx = test_tool_env(workspace, gateway, Arc::clone(&approvals), BTreeMap::new());
        let registry = default_factory()
            .build_registry(&only_read_enabled(), &[])
            .expect("tool registry should build");
        let entry = registry
            .get("read")
            .expect("read tool should exist")
            .clone();
        let wasm = Arc::new(StubWasmSandbox {
            calls: AtomicUsize::new(0),
            stderr: "path denied by sandbox".to_owned(),
        });
        let executor = DefaultToolExecutor::with_runtimes(wasm.clone(), Arc::new(PanicHostSandbox));

        let approvals_task = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if let Some(request) = approvals.pending_requests().await.into_iter().next() {
                        let _ = approvals
                            .resolve(
                                &request.approval_id,
                                &request.requesting_user_id,
                                ApprovalDecision::Denied,
                            )
                            .await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("approval request was never created");
            })
        };

        let err = executor
            .execute(
                &entry,
                &ctx,
                &format!(
                    r#"{{"filePath":"{}"}}"#,
                    outside.join("notes.txt").display()
                ),
                "sess-1",
            )
            .await
            .expect_err("denied approval should fail");
        approvals_task.await.expect("approval task should finish");

        assert!(matches!(err, FrameworkError::ApprovalDenied { .. }));
        assert_eq!(wasm.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn in_sandbox_wasm_denial_is_returned_as_invariant_error() {
        let workspace = unique_temp_dir("approval_skip");
        std::fs::write(workspace.join("notes.txt"), "hello\n").expect("test file should exist");
        let approvals = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(CaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let ctx = test_tool_env(workspace, gateway, Arc::clone(&approvals), BTreeMap::new());
        let registry = default_factory()
            .build_registry(&only_read_enabled(), &[])
            .expect("tool registry should build");
        let entry = registry
            .get("read")
            .expect("read tool should exist")
            .clone();
        let wasm = Arc::new(StubWasmSandbox {
            calls: AtomicUsize::new(0),
            stderr: "path denied by sandbox: path=/workspace/notes.txt workspace=/workspace persona=/persona".to_owned(),
        });
        let executor = DefaultToolExecutor::with_runtimes(wasm.clone(), Arc::new(PanicHostSandbox));

        let err = executor
            .execute(&entry, &ctx, r#"{"filePath":"notes.txt"}"#, "sess-1")
            .await
            .expect_err("sandbox invariant failures should hard error");

        assert!(
            err.to_string()
                .contains("sandbox invariant violated for read")
        );
        assert_eq!(wasm.calls.load(Ordering::SeqCst), 1);
        assert!(approvals.pending_requests().await.is_empty());
        assert!(channel.sent_messages.lock().await.is_empty());
    }

    #[tokio::test]
    async fn outside_sandbox_edit_ignores_wrong_user_approval_until_requester_approves() {
        let workspace = unique_temp_dir("approval_wrong_user");
        let outside = unique_temp_dir("approval_wrong_user_outside");
        std::fs::write(outside.join("notes.txt"), "hello\n").expect("test file should exist");
        let approvals = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(CaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let ctx = test_tool_env(workspace, gateway, Arc::clone(&approvals), BTreeMap::new());
        let registry = default_factory()
            .build_registry(&only_edit_enabled(), &[])
            .expect("tool registry should build");
        let entry = registry
            .get("edit")
            .expect("edit tool should exist")
            .clone();
        let wasm = Arc::new(StubWasmSandbox {
            calls: AtomicUsize::new(0),
            stderr: "path denied by sandbox".to_owned(),
        });
        let executor = DefaultToolExecutor::with_runtimes(wasm.clone(), Arc::new(PanicHostSandbox));

        let approvals_task = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if let Some(request) = approvals.pending_requests().await.into_iter().next() {
                        let wrong_user_resolved = approvals
                            .resolve(
                                &request.approval_id,
                                "intruder-1",
                                ApprovalDecision::Approved,
                            )
                            .await;
                        assert!(!wrong_user_resolved);
                        assert_eq!(approvals.pending_requests().await.len(), 1);
                        let requester_resolved = approvals
                            .resolve(
                                &request.approval_id,
                                &request.requesting_user_id,
                                ApprovalDecision::Approved,
                            )
                            .await;
                        assert!(requester_resolved);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("approval request was never created");
            })
        };

        let outcome = executor
            .execute(
                &entry,
                &ctx,
                &format!(
                    r#"{{"filePath":"{}","oldString":"hello","newString":"updated"}}"#,
                    outside.join("notes.txt").display()
                ),
                "sess-1",
            )
            .await
            .expect("requesting user approval should allow direct retry");
        approvals_task.await.expect("approval task should finish");

        let ToolExecutionOutcome::Completed(output) = outcome else {
            panic!("expected completed tool output");
        };
        let sent = channel.sent_messages.lock().await.clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Only the requesting user may approve or deny"));
        assert!(sent[0].contains("owner-1"));
        assert_eq!(wasm.calls.load(Ordering::SeqCst), 0);
        assert!(output.output.contains("Edit applied successfully"));
    }

    #[tokio::test]
    async fn exec_preflight_requests_approval_before_unsandboxed_retry() {
        let workspace = unique_temp_dir("exec_approval");
        let approvals = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(CaptureChannel::default());
        let gateway = test_gateway(channel.clone());
        let ctx = test_tool_env(
            workspace,
            gateway,
            Arc::clone(&approvals),
            BTreeMap::from([(
                "SIMPLECLAW_EXEC_DYNAMIC".to_owned(),
                "printf hello-from-approval".to_owned(),
            )]),
        );
        let registry = default_factory()
            .build_registry(&only_exec_enabled(), &[])
            .expect("tool registry should build");
        let entry = registry
            .get("exec")
            .expect("exec tool should exist")
            .clone();
        let executor = DefaultToolExecutor::with_runtimes(
            Arc::new(StubWasmSandbox {
                calls: AtomicUsize::new(0),
                stderr: String::new(),
            }),
            Arc::new(PanicHostSandbox),
        );

        let approvals_task = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if let Some(request) = approvals.pending_requests().await.into_iter().next() {
                        let _ = approvals
                            .resolve(
                                &request.approval_id,
                                &request.requesting_user_id,
                                ApprovalDecision::Approved,
                            )
                            .await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("approval request was never created");
            })
        };

        let outcome = executor
            .execute(
                &entry,
                &ctx,
                r#"{"command":"bash -c \"$SIMPLECLAW_EXEC_DYNAMIC\""}"#,
                "sess-1",
            )
            .await
            .expect("approval should allow unsandboxed direct retry");
        approvals_task.await.expect("approval task should finish");

        let ToolExecutionOutcome::Completed(output) = outcome else {
            panic!("expected completed tool output");
        };
        assert!(output.output.contains("hello-from-approval"));
        let sent = channel.sent_messages.lock().await.clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Approval required."));
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
