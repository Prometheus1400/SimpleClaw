use async_trait::async_trait;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ReadToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::file_access::{
    FileToolRoute, classify_file_tool_access, classify_wasm_tool_error, resolve_path_for_read,
};

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadTool {
    config: ReadToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub host_path: PathBuf,
    pub route: FileToolRoute,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    #[serde(rename = "filePath")]
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file or list a directory. Returns numbered lines. Use offset and limit to page through large files. Lines are 1-indexed; default limit is 2000."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"filePath\":{\"type\":\"string\",\"description\":\"Absolute or workspace-relative path.\"},\"offset\":{\"type\":\"integer\",\"minimum\":1,\"description\":\"1-indexed line number to start from. Defaults to 1.\"},\"limit\":{\"type\":\"integer\",\"minimum\":1,\"description\":\"Max number of lines to return. Defaults to 2000.\"}},\"required\":[\"filePath\"]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.read config is invalid: {e}")))?;
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

impl ReadTool {
    pub fn plan(&self, ctx: &ToolExecEnv<'_>, args_json: &str) -> Result<ReadPlan, FrameworkError> {
        let args: ReadArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("read requires JSON object args: {e}")))?;
        let file_path = args.file_path.trim();
        if file_path.is_empty() {
            return Err(FrameworkError::Tool(
                "read requires a non-empty filePath".to_owned(),
            ));
        }
        let offset = args.offset.unwrap_or(1);
        if offset == 0 {
            return Err(FrameworkError::Tool(
                "read offset must be greater than or equal to 1".to_owned(),
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if limit == 0 {
            return Err(FrameworkError::Tool(
                "read limit must be greater than or equal to 1".to_owned(),
            ));
        }

        let host_path = resolve_path_for_read(file_path, &ctx.workspace_root)?;
        let route = classify_file_tool_access(
            &host_path,
            &ctx.workspace_root,
            &ctx.persona_root,
            &self.config.sandbox,
            SandboxCapability::Read,
        )?;
        Ok(ReadPlan {
            host_path,
            route,
            offset,
            limit,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv<'_>,
        plan: ReadPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let rendered = if plan.host_path.is_dir() {
            render_directory_output(&plan.host_path, plan.offset, plan.limit)?
        } else {
            render_file_output(&plan.host_path, plan.offset, plan.limit)?
        };
        Ok(ToolRunOutput::plain(rendered))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: ReadPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "path": plan
                .route
                .guest_path()
                .ok_or_else(|| FrameworkError::Tool("read plan is not sandbox-runnable".to_owned()))?,
            "offset": plan.offset,
            "limit": plan.limit,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize read args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.to_path_buf(),
                persona_root: ctx.persona_root.to_path_buf(),
                preopened_dirs: plan.route.preopened_dirs().to_vec(),
                artifact_name: "read_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(10)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(classify_wasm_tool_error(
                "read",
                plan.route.guest_path().unwrap_or(""),
                SandboxCapability::Read,
                output.exit_code,
                output.stderr.trim(),
            ));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

fn render_file_output(path: &Path, offset: usize, limit: usize) -> Result<String, FrameworkError> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        if offset != 1 {
            return Err(FrameworkError::Tool(format!(
                "Offset {offset} is out of range for this file (0 lines)"
            )));
        }
    } else if offset > total_lines {
        return Err(FrameworkError::Tool(format!(
            "Offset {offset} is out of range for this file ({total_lines} lines)"
        )));
    }

    let start = offset.saturating_sub(1);
    let selected = lines
        .iter()
        .skip(start)
        .take(limit)
        .enumerate()
        .map(|(idx, line)| {
            let value = if line.chars().count() > MAX_LINE_LENGTH {
                format!(
                    "{}...",
                    line.chars().take(MAX_LINE_LENGTH).collect::<String>()
                )
            } else {
                (*line).to_owned()
            };
            format!("{}: {}", idx + offset, value)
        })
        .collect::<Vec<_>>();

    let last_read_line = if selected.is_empty() {
        offset.saturating_sub(1)
    } else {
        offset + selected.len() - 1
    };
    let has_more = last_read_line < total_lines;
    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>file</type>".to_owned(),
        "<content>".to_owned(),
    ];
    output.extend(selected);

    if has_more {
        output.push(String::new());
        output.push(format!(
            "(Showing lines {}-{} of {}. Use offset={} to continue.)",
            offset,
            last_read_line,
            total_lines,
            last_read_line + 1
        ));
    } else {
        output.push(String::new());
        output.push(format!("(End of file - total {} lines)", total_lines));
    }
    output.push("</content>".to_owned());
    Ok(output.join("\n"))
}

fn render_directory_output(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<String, FrameworkError> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| {
            let entry = entry.map_err(FrameworkError::Io)?;
            let mut name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| FrameworkError::Tool("directory entry is not utf-8".to_owned()))?
                .to_owned();
            if entry.file_type().map_err(FrameworkError::Io)?.is_dir() {
                name.push('/');
            }
            Ok(name)
        })
        .collect::<Result<Vec<_>, FrameworkError>>()?;
    entries.sort();

    if offset == 0 {
        return Err(FrameworkError::Tool(
            "read offset must be greater than or equal to 1".to_owned(),
        ));
    }
    let start = offset - 1;
    let selected = entries
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let truncated = start + selected.len() < entries.len();

    let mut output = vec![
        format!("<path>{}</path>", path.display()),
        "<type>directory</type>".to_owned(),
        "<entries>".to_owned(),
        selected.join("\n"),
    ];
    if truncated {
        output.push(format!(
            "\n(Showing {} of {} entries. Use offset={} to continue.)",
            selected.len(),
            entries.len(),
            offset + selected.len()
        ));
    } else {
        output.push(format!("\n({} entries)", entries.len()));
    }
    output.push("</entries>".to_owned());
    Ok(output.join("\n"))
}
