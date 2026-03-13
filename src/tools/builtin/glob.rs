use async_trait::async_trait;
use glob::Pattern;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::GlobToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::file_access::{
    FileToolRoute, classify_file_tool_access, classify_wasm_tool_error, resolve_path_for_read,
};

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobTool {
    config: GlobToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobPlan {
    pub pattern: String,
    pub host_path: PathBuf,
    pub route: FileToolRoute,
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by pattern matching using JSON: {pattern, path?}. Returns matching absolute paths sorted by modification time."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"pattern\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"}},\"required\":[\"pattern\"]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.glob config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan)
            .await
            .map(ToolExecutionOutcome::Completed)
    }
}

impl GlobTool {
    pub fn plan(&self, ctx: &ToolExecEnv<'_>, args_json: &str) -> Result<GlobPlan, FrameworkError> {
        let args: GlobArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("glob requires JSON object args: {e}")))?;
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(FrameworkError::Tool(
                "glob requires a non-empty pattern".to_owned(),
            ));
        }
        let raw_path = args.path.as_deref().unwrap_or(".");
        let host_path = resolve_path_for_read(raw_path, &ctx.workspace_root)?;
        if !host_path.is_dir() {
            return Err(FrameworkError::Tool(format!(
                "glob path must be a directory: {}",
                host_path.display()
            )));
        }
        let route = classify_file_tool_access(
            &host_path,
            &ctx.workspace_root,
            &ctx.persona_root,
            &self.config.sandbox,
            SandboxCapability::Read,
        )?;
        Ok(GlobPlan {
            pattern: pattern.to_owned(),
            host_path,
            route,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv<'_>,
        plan: GlobPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let output = run_glob(&plan.pattern, &plan.host_path)?;
        Ok(ToolRunOutput::plain(output))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: GlobPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "pattern": plan.pattern,
            "path": plan
                .route
                .guest_path()
                .ok_or_else(|| FrameworkError::Tool("glob plan is not sandbox-runnable".to_owned()))?,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize glob args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.to_path_buf(),
                persona_root: ctx.persona_root.to_path_buf(),
                preopened_dirs: plan.route.preopened_dirs().to_vec(),
                artifact_name: "glob_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(15)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(classify_wasm_tool_error(
                "glob",
                plan.route.guest_path().unwrap_or(""),
                SandboxCapability::Read,
                output.exit_code,
                output.stderr.trim(),
            ));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

pub(crate) fn run_glob(pattern: &str, root: &Path) -> Result<String, FrameworkError> {
    let pattern = Pattern::new(pattern)
        .map_err(|e| FrameworkError::Tool(format!("glob pattern is invalid: {e}")))?;
    let mut stack = vec![root.to_path_buf()];
    let mut matches: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| {
            FrameworkError::Tool(format!("glob failed to read {}: {e}", dir.display()))
        })?;
        for entry in entries {
            let entry = entry
                .map_err(|e| FrameworkError::Tool(format!("glob failed to walk directory: {e}")))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| {
                FrameworkError::Tool(format!("glob failed to inspect {}: {e}", path.display()))
            })?;
            if file_type.is_dir() {
                stack.push(path.clone());
            }
            let relative = path.strip_prefix(root).map_err(|e| {
                FrameworkError::Tool(format!("glob failed to compute relative path: {e}"))
            })?;
            if pattern.matches_path(relative) {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                matches.push((path, modified));
            }
        }
    }

    matches.sort_by(|a, b| b.1.cmp(&a.1));
    if matches.is_empty() {
        return Ok("No files found".to_owned());
    }
    let mut lines = Vec::new();
    for (path, _) in matches.into_iter().take(DEFAULT_LIMIT) {
        lines.push(path.display().to_string());
    }
    Ok(lines.join("\n"))
}
