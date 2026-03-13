use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::config::EditToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::file_access::{
    FileToolRoute, classify_file_tool_access, classify_wasm_tool_error, resolve_path_for_read,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditTool {
    config: EditToolConfig,
}

#[derive(Debug, Clone)]
pub struct EditPlan {
    pub args: EditArgs,
    pub host_path: std::path::PathBuf,
    pub route: FileToolRoute,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EditArgs {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "replaceAll", default)]
    pub replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit a file using JSON: {filePath, oldString, newString, replaceAll?}."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"filePath\":{\"type\":\"string\"},\"oldString\":{\"type\":\"string\"},\"newString\":{\"type\":\"string\"},\"replaceAll\":{\"type\":\"boolean\"}},\"required\":[\"filePath\",\"oldString\",\"newString\"]}"
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::WasmSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.edit config is invalid: {e}")))?;
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

impl EditTool {
    pub fn plan(&self, ctx: &ToolExecEnv<'_>, args_json: &str) -> Result<EditPlan, FrameworkError> {
        let args: EditArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("edit requires JSON object args: {e}")))?;
        if args.file_path.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "edit requires a non-empty filePath".to_owned(),
            ));
        }
        if args.old_string == args.new_string {
            return Err(FrameworkError::Tool(
                "No changes to apply: oldString and newString are identical.".to_owned(),
            ));
        }
        let host_path = resolve_path_for_read(&args.file_path, &ctx.workspace_root)?;
        let route = classify_file_tool_access(
            &host_path,
            &ctx.workspace_root,
            &ctx.persona_root,
            &self.config.sandbox,
            SandboxCapability::Write,
        )?;
        Ok(EditPlan {
            args,
            host_path,
            route,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv<'_>,
        plan: EditPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        apply_edit_at_path(&plan.host_path, &plan.args)
            .map(|output| ToolRunOutput::plain(output.to_owned()))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: EditPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let stdin = serde_json::to_vec(&serde_json::json!({
            "filePath": plan
                .route
                .guest_path()
                .ok_or_else(|| FrameworkError::Tool("edit plan is not sandbox-runnable".to_owned()))?,
            "oldString": plan.args.old_string,
            "newString": plan.args.new_string,
            "replaceAll": plan.args.replace_all,
        }))
        .map_err(|e| FrameworkError::Tool(format!("failed to serialize edit args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.to_path_buf(),
                persona_root: ctx.persona_root.to_path_buf(),
                preopened_dirs: plan.route.preopened_dirs().to_vec(),
                artifact_name: "edit_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(15)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(classify_wasm_tool_error(
                "edit",
                plan.route.guest_path().unwrap_or(""),
                SandboxCapability::Write,
                output.exit_code,
                output.stderr.trim(),
            ));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

fn apply_edit_at_path(path: &Path, args: &EditArgs) -> Result<&'static str, FrameworkError> {
    if args.old_string.is_empty() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &args.new_string)?;
        return Ok("Edit applied successfully.");
    }

    let original = fs::read_to_string(path)
        .map_err(|e| FrameworkError::Tool(format!("File {} not found: {e}", path.display())))?;
    let ending = detect_line_ending(&original);
    let old_string = convert_to_line_ending(&normalize_line_endings(&args.old_string), ending);
    let new_string = convert_to_line_ending(&normalize_line_endings(&args.new_string), ending);

    let occurrences = original.match_indices(&old_string).count();
    if occurrences == 0 {
        return Err(FrameworkError::Tool(
            "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.".to_owned(),
        ));
    }
    if occurrences > 1 && !args.replace_all {
        return Err(FrameworkError::Tool(
            "Found multiple matches for oldString. Provide more surrounding context to make the match unique."
                .to_owned(),
        ));
    }

    let updated = if args.replace_all {
        original.replace(&old_string, &new_string)
    } else {
        original.replacen(&old_string, &new_string, 1)
    };
    fs::write(path, updated)?;
    Ok("Edit applied successfully.")
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn convert_to_line_ending(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_owned()
    } else {
        text.replace('\n', "\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::{EditArgs, apply_edit_at_path};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_edit_{prefix}_{nanos}"))
    }

    #[test]
    fn create_file_when_old_string_is_empty() {
        let workspace = unique_test_dir("create");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");

        let args = EditArgs {
            file_path: path.display().to_string(),
            old_string: String::new(),
            new_string: "hello\n".to_owned(),
            replace_all: false,
        };
        apply_edit_at_path(&path, &args).expect("edit should succeed");
        assert_eq!(fs::read_to_string(&path).expect("should read"), "hello\n");
    }

    #[test]
    fn replace_requires_unique_match_without_replace_all() {
        let workspace = unique_test_dir("replace");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");
        fs::write(&path, "a b a\n").expect("write should succeed");

        let args = EditArgs {
            file_path: path.display().to_string(),
            old_string: "a".to_owned(),
            new_string: "z".to_owned(),
            replace_all: false,
        };
        let err = apply_edit_at_path(&path, &args).expect_err("edit should fail");
        assert!(
            err.to_string()
                .contains("Found multiple matches for oldString")
        );
    }
}
