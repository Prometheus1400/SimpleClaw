pub mod builtin;
pub mod sandbox;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::config::{SandboxMode, ToolConfig};
use crate::error::FrameworkError;
use crate::memory::MemoryStore;
use crate::provider::ToolDefinition;

#[async_trait]
pub trait SummonService: Send + Sync {
    async fn summon(
        &self,
        target_agent: &str,
        summary: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError>;
}

#[async_trait]
pub trait TaskService: Send + Sync {
    async fn run_task(&self, prompt: &str, session_id: &str) -> Result<String, FrameworkError>;
}

#[derive(Clone)]
pub struct ToolCtx {
    pub memory: MemoryStore,
    pub sandbox: SandboxMode,
    pub workspace_root: PathBuf,
    pub user_id: String,
    pub owner_ids: Vec<String>,
    pub process_manager: Arc<ProcessManager>,
    pub summon_service: Option<Arc<dyn SummonService>>,
    pub task_service: Option<Arc<dyn TaskService>>,
}

impl ToolCtx {
    pub fn owner_allowed(user_id: &str, owner_ids: &[String]) -> bool {
        !owner_ids.is_empty() && owner_ids.iter().any(|owner_id| owner_id == user_id)
    }

    pub fn is_owner(&self) -> bool {
        Self::owner_allowed(&self.user_id, &self.owner_ids)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema_json(&self) -> &'static str;

    async fn execute(
        &self,
        ctx: &ToolCtx,
        args_json: &str,
        session_id: &str,
    ) -> Result<String, FrameworkError>;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            input_schema_json: self.input_schema_json().to_owned(),
        }
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    ordered: Vec<Arc<dyn Tool>>,
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool));
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_owned();
        if let Some(existing_idx) = self
            .ordered
            .iter()
            .position(|candidate| candidate.name() == name)
        {
            self.ordered[existing_idx] = Arc::clone(&tool);
        } else {
            self.ordered.push(Arc::clone(&tool));
        }
        self.by_name.insert(name, tool);
    }

    pub fn resolve_active(&self, config: &ToolConfig) -> Result<ActiveTools, FrameworkError> {
        let mut ordered = Vec::new();
        let mut by_name = HashMap::new();
        let mut seen = HashSet::new();

        for name in &config.enabled_tools {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(tool) = self.by_name.get(name) else {
                return Err(FrameworkError::Config(format!(
                    "unknown tool in tools.enabled_tools: {name}"
                )));
            };
            ordered.push(Arc::clone(tool));
            by_name.insert(name.clone(), Arc::clone(tool));
        }

        Ok(ActiveTools { ordered, by_name })
    }
}

pub struct ActiveTools {
    ordered: Vec<Arc<dyn Tool>>,
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl ActiveTools {
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.ordered.iter().map(|tool| tool.definition()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.ordered.iter().map(|tool| tool.name()).collect()
    }
}

pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    builtin::register_builtin_tools(&mut registry);
    registry
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
    ) -> Result<String, FrameworkError> {
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

        let mut child = Command::new("bash");
        child.arg("-lc").arg(command);
        if let Some(workspace_root) = workspace_root {
            child.current_dir(workspace_root);
        }
        child
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        let child = child
            .spawn()
            .map_err(|e| FrameworkError::Tool(format!("exec failed to start: {e}")))?;

        let started_at = Utc::now();
        let pid = child.id();
        let entry = ProcessEntry {
            command: command.to_owned(),
            status: ProcessStatus::Running,
            started_at,
            finished_at: None,
            pid,
            child: Some(child),
            stdout_path,
            stderr_path,
            exit_code: None,
        };

        self.sessions.lock().await.insert(session_id.clone(), entry);
        Ok(session_id)
    }

    pub async fn update(&self, session_id: &str) -> Result<ProcessSnapshot, FrameworkError> {
        let mut sessions = self.sessions.lock().await;
        let entry = sessions.get_mut(session_id).ok_or_else(|| {
            FrameworkError::Tool(format!("unknown process session_id: {session_id}"))
        })?;
        entry.poll_completion().await;
        Ok(entry.snapshot(session_id.to_owned()))
    }

    pub async fn list(&self) -> Vec<ProcessSnapshot> {
        let mut sessions = self.sessions.lock().await;
        let ids: Vec<String> = sessions.keys().cloned().collect();
        for id in &ids {
            if let Some(entry) = sessions.get_mut(id) {
                entry.poll_completion().await;
            }
        }
        let mut items = sessions
            .iter()
            .map(|(id, entry)| entry.snapshot(id.clone()))
            .collect::<Vec<_>>();
        items.sort_by_key(|snapshot| snapshot.started_at);
        items.reverse();
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
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct ProcessEntry {
    command: String,
    status: ProcessStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    pid: Option<u32>,
    child: Option<Child>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    exit_code: Option<i32>,
}

impl ProcessEntry {
    async fn poll_completion(&mut self) {
        if self.status != ProcessStatus::Running {
            return;
        }
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = self.child.take();
                self.exit_code = status.code();
                self.status = ProcessStatus::Completed;
                self.finished_at = Some(Utc::now());
            }
            Ok(None) => {}
            Err(_) => {
                self.status = ProcessStatus::Completed;
                self.finished_at = Some(Utc::now());
            }
        }
    }

    async fn kill(&mut self) -> Result<(), FrameworkError> {
        if self.status != ProcessStatus::Running {
            return Ok(());
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        child
            .kill()
            .await
            .map_err(|e| FrameworkError::Tool(format!("failed to kill process: {e}")))?;
        let _ = child.wait().await;
        self.status = ProcessStatus::Killed;
        self.finished_at = Some(Utc::now());
        self.exit_code = Some(-1);
        let _ = self.child.take();
        Ok(())
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
            stdout: read_process_output(&self.stdout_path),
            stderr: read_process_output(&self.stderr_path),
        }
    }
}

fn read_process_output(path: &std::path::Path) -> String {
    let mut content = String::new();
    if let Ok(mut file) = File::open(path) {
        let _ = file.read_to_string(&mut content);
    }
    content
}

pub async fn wait_for_completion(
    process_manager: &ProcessManager,
    session_id: &str,
    wait_for: Duration,
) -> Result<ProcessSnapshot, FrameworkError> {
    let deadline = std::time::Instant::now() + wait_for;
    loop {
        let snapshot = process_manager.update(session_id).await?;
        if snapshot.status != ProcessStatus::Running {
            return Ok(snapshot);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(snapshot);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeToolA;
    struct FakeToolB;

    #[async_trait]
    impl Tool for FakeToolA {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn description(&self) -> &'static str {
            "fake-a"
        }

        fn input_schema_json(&self) -> &'static str {
            "{\"type\":\"null\"}"
        }

        async fn execute(
            &self,
            _ctx: &ToolCtx,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<String, FrameworkError> {
            Ok("a".to_owned())
        }
    }

    #[async_trait]
    impl Tool for FakeToolB {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn description(&self) -> &'static str {
            "fake-b"
        }

        fn input_schema_json(&self) -> &'static str {
            "{\"type\":\"null\"}"
        }

        async fn execute(
            &self,
            _ctx: &ToolCtx,
            _args_json: &str,
            _session_id: &str,
        ) -> Result<String, FrameworkError> {
            Ok("b".to_owned())
        }
    }

    #[test]
    fn resolve_active_rejects_unknown_tool_names() {
        let registry = default_registry();
        let config = ToolConfig {
            enabled_tools: vec!["not_real".to_owned()],
        };

        let result = registry.resolve_active(&config);
        assert!(result.is_err());
    }

    #[test]
    fn register_overwrites_existing_tool_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(FakeToolA);
        registry.register(FakeToolB);

        let active = registry
            .resolve_active(&ToolConfig {
                enabled_tools: vec!["fake".to_owned()],
            })
            .expect("fake tool should resolve");
        let definitions = active.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].description, "fake-b");
    }

    #[test]
    fn tool_ctx_owner_allowed_when_owner_ids_empty_is_false() {
        assert!(!ToolCtx::owner_allowed("user-1", &[]));
    }

    #[test]
    fn tool_ctx_owner_allowed_when_user_matches() {
        let owner_ids = vec!["owner-1".to_owned(), "owner-2".to_owned()];
        assert!(ToolCtx::owner_allowed("owner-2", &owner_ids));
    }

    #[test]
    fn tool_ctx_owner_allowed_when_user_missing() {
        let owner_ids = vec!["owner-1".to_owned(), "owner-2".to_owned()];
        assert!(!ToolCtx::owner_allowed("user-3", &owner_ids));
    }
}
