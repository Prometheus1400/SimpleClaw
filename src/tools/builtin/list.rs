use async_trait::async_trait;
use glob::Pattern;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ListToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::file_access::{
    FileToolRoute, classify_file_tool_access, classify_wasm_tool_error, resolve_path_for_read,
};

const LIMIT: usize = 100;
const IGNORE_PATTERNS: &[&str] = &[
    "node_modules/**",
    "__pycache__/**",
    ".git/**",
    "dist/**",
    "build/**",
    "target/**",
    "vendor/**",
    "bin/**",
    "obj/**",
    ".idea/**",
    ".vscode/**",
    ".cache/**",
    "cache/**",
    "logs/**",
    ".venv/**",
    "venv/**",
    "env/**",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListTool {
    config: ListToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPlan {
    pub host_path: PathBuf,
    pub route: FileToolRoute,
    pub ignore: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    path: Option<String>,
    ignore: Option<Vec<String>>,
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &'static str {
        "list"
    }

    fn description(&self) -> &'static str {
        "List files and directories using JSON: {path?, ignore?}."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"ignore\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"required\":[]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.list config is invalid: {e}")))?;
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

impl ListTool {
    pub fn plan(&self, ctx: &ToolExecEnv<'_>, args_json: &str) -> Result<ListPlan, FrameworkError> {
        let args: ListArgs = if args_json.trim().is_empty() {
            ListArgs {
                path: None,
                ignore: None,
            }
        } else {
            serde_json::from_str(args_json)
                .map_err(|e| FrameworkError::Tool(format!("list requires JSON object args: {e}")))?
        };

        let raw_path = args.path.as_deref().unwrap_or(".");
        let host_path = resolve_path_for_read(raw_path, &ctx.workspace_root)?;
        if !host_path.is_dir() {
            return Err(FrameworkError::Tool(format!(
                "list path must be a directory: {}",
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
        Ok(ListPlan {
            host_path,
            route,
            ignore: args.ignore.unwrap_or_default(),
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv<'_>,
        plan: ListPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let output = run_list(&plan.host_path, &plan.ignore)?;
        Ok(ToolRunOutput::plain(output))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: ListPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "path": plan
                .route
                .guest_path()
                .ok_or_else(|| FrameworkError::Tool("list plan is not sandbox-runnable".to_owned()))?,
            "ignore": plan.ignore,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize list args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.to_path_buf(),
                persona_root: ctx.persona_root.to_path_buf(),
                preopened_dirs: plan.route.preopened_dirs().to_vec(),
                artifact_name: "list_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(15)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(classify_wasm_tool_error(
                "list",
                plan.route.guest_path().unwrap_or(""),
                SandboxCapability::Read,
                output.exit_code,
                output.stderr.trim(),
            ));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

pub(crate) fn run_list(root: &Path, ignore: &[String]) -> Result<String, FrameworkError> {
    let mut ignore_patterns = Vec::new();
    for pattern in IGNORE_PATTERNS {
        ignore_patterns.push(
            Pattern::new(pattern).map_err(|e| {
                FrameworkError::Tool(format!("invalid built-in ignore pattern: {e}"))
            })?,
        );
    }
    for pattern in ignore {
        ignore_patterns.push(Pattern::new(pattern).map_err(|e| {
            FrameworkError::Tool(format!("invalid ignore pattern '{pattern}': {e}"))
        })?);
    }

    let mut dirs = BTreeSet::new();
    let mut files_by_dir: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    dirs.insert(PathBuf::from("."));

    let mut files_seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| {
            FrameworkError::Tool(format!("list failed to read {}: {e}", dir.display()))
        })?;
        for entry in entries {
            if files_seen >= LIMIT {
                break;
            }
            let entry = entry
                .map_err(|e| FrameworkError::Tool(format!("list failed to walk directory: {e}")))?;
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|e| {
                FrameworkError::Tool(format!("list failed to compute relative path: {e}"))
            })?;
            if is_ignored(relative, &ignore_patterns) {
                continue;
            }
            let file_type = entry.file_type().map_err(|e| {
                FrameworkError::Tool(format!("list failed to inspect {}: {e}", path.display()))
            })?;
            if file_type.is_dir() {
                stack.push(path.clone());
                dirs.insert(relative.to_path_buf());
                let mut parent = relative.parent();
                while let Some(next) = parent {
                    dirs.insert(if next.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        next.to_path_buf()
                    });
                    parent = next.parent();
                }
            } else if file_type.is_file() {
                files_seen += 1;
                let parent = relative
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let name = relative
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or_default()
                    .to_owned();
                files_by_dir.entry(parent).or_default().push(name);
            }
        }
    }

    let mut output = String::new();
    output.push_str(&format!("{}/\n", root.display()));
    output.push_str(&render_dir(Path::new("."), 0, &dirs, &files_by_dir));
    Ok(output)
}

fn render_dir(
    dir: &Path,
    depth: usize,
    dirs: &BTreeSet<PathBuf>,
    files_by_dir: &BTreeMap<PathBuf, Vec<String>>,
) -> String {
    let mut output = String::new();
    if depth > 0 {
        let indent = "  ".repeat(depth);
        output.push_str(&format!(
            "{}{}/\n",
            indent,
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("")
        ));
    }

    let children = dirs
        .iter()
        .filter(|candidate| {
            candidate != &&PathBuf::from(".")
                && candidate.parent().unwrap_or_else(|| Path::new(".")) == dir
        })
        .cloned()
        .collect::<Vec<_>>();

    for child in children {
        output.push_str(&render_dir(&child, depth + 1, dirs, files_by_dir));
    }

    let dir_key = if dir == Path::new(".") {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    };
    if let Some(files) = files_by_dir.get(&dir_key) {
        let indent = "  ".repeat(depth + 1);
        let mut files = files.clone();
        files.sort();
        for file in files {
            output.push_str(&format!("{}{}\n", indent, file));
        }
    }

    output
}

fn is_ignored(path: &Path, patterns: &[Pattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches_path(path))
}
