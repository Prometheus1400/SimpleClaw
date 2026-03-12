use async_trait::async_trait;
use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::GrepToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::file_access::{
    FileToolRoute, classify_file_tool_access, classify_wasm_tool_error, resolve_path_for_read,
};

const MAX_LINE_LENGTH: usize = 2000;
const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrepTool {
    config: GrepToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepPlan {
    pub pattern: String,
    pub include: Option<String>,
    pub host_path: PathBuf,
    pub route: FileToolRoute,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
}

#[derive(Debug)]
struct GrepMatch {
    path: PathBuf,
    line_number: usize,
    line_text: String,
    modified: SystemTime,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents using regex with JSON: {pattern, path?, include?}."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"pattern\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"include\":{\"type\":\"string\"}},\"required\":[\"pattern\"]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.grep config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv,
        args_json: &str,
        _session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan)
            .await
            .map(ToolExecutionOutcome::Completed)
    }
}

impl GrepTool {
    pub fn plan(&self, ctx: &ToolExecEnv, args_json: &str) -> Result<GrepPlan, FrameworkError> {
        let args: GrepArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("grep requires JSON object args: {e}")))?;
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(FrameworkError::Tool(
                "grep requires a non-empty pattern".to_owned(),
            ));
        }
        let raw_path = args.path.as_deref().unwrap_or(".");
        let host_path = resolve_path_for_read(raw_path, &ctx.workspace_root)?;
        if !host_path.is_dir() {
            return Err(FrameworkError::Tool(format!(
                "grep path must be a directory: {}",
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
        Ok(GrepPlan {
            pattern: pattern.to_owned(),
            include: args.include,
            host_path,
            route,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv,
        plan: GrepPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let output = run_grep(&plan.pattern, &plan.host_path, plan.include.as_deref())?;
        Ok(ToolRunOutput::plain(output))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv,
        plan: GrepPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "pattern": plan.pattern,
            "path": plan
                .route
                .guest_path()
                .ok_or_else(|| FrameworkError::Tool("grep plan is not sandbox-runnable".to_owned()))?,
            "include": plan.include,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize grep args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.clone(),
                persona_root: ctx.persona_root.clone(),
                preopened_dirs: plan.route.preopened_dirs().to_vec(),
                artifact_name: "grep_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(20)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(classify_wasm_tool_error(
                "grep",
                plan.route.guest_path().unwrap_or(""),
                SandboxCapability::Read,
                output.exit_code,
                output.stderr.trim(),
            ));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

pub(crate) fn run_grep(
    pattern: &str,
    root: &Path,
    include: Option<&str>,
) -> Result<String, FrameworkError> {
    let regex = Regex::new(pattern)
        .map_err(|e| FrameworkError::Tool(format!("grep pattern is invalid regex: {e}")))?;
    let include = include
        .map(Pattern::new)
        .transpose()
        .map_err(|e| FrameworkError::Tool(format!("grep include pattern is invalid: {e}")))?;

    let files = collect_files(root)?;
    let mut matches = Vec::new();
    for path in files {
        let relative = path.strip_prefix(root).map_err(|e| {
            FrameworkError::Tool(format!("grep failed to compute relative path: {e}"))
        })?;
        if let Some(include_pattern) = include.as_ref()
            && !include_pattern.matches_path(relative)
        {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let modified = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                let line_text = if line.chars().count() > MAX_LINE_LENGTH {
                    format!(
                        "{}...",
                        line.chars().take(MAX_LINE_LENGTH).collect::<String>()
                    )
                } else {
                    line.to_owned()
                };
                matches.push(GrepMatch {
                    path: path.clone(),
                    line_number: idx + 1,
                    line_text,
                    modified,
                });
            }
        }
    }

    if matches.is_empty() {
        return Ok("No files found".to_owned());
    }
    matches.sort_by(|a, b| b.modified.cmp(&a.modified));
    let total = matches.len();
    let truncated = total > DEFAULT_LIMIT;
    let selected = matches.into_iter().take(DEFAULT_LIMIT).collect::<Vec<_>>();
    let mut out = vec![if truncated {
        format!("Found {total} matches (showing first {DEFAULT_LIMIT})")
    } else {
        format!("Found {total} matches")
    }];

    let mut current: Option<PathBuf> = None;
    for item in selected {
        if current.as_ref() != Some(&item.path) {
            if current.is_some() {
                out.push(String::new());
            }
            current = Some(item.path.clone());
            out.push(format!("{}:", item.path.display()));
        }
        out.push(format!("  Line {}: {}", item.line_number, item.line_text));
    }
    Ok(out.join("\n"))
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, FrameworkError> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| {
            FrameworkError::Tool(format!("grep failed to read {}: {e}", dir.display()))
        })?;
        for entry in entries {
            let entry = entry
                .map_err(|e| FrameworkError::Tool(format!("grep failed to walk directory: {e}")))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| {
                FrameworkError::Tool(format!("grep failed to inspect {}: {e}", path.display()))
            })?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}
