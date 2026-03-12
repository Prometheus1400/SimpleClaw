use async_trait::async_trait;
use sandbox_common::{EditArgs, apply_edit_command_at_path};
#[cfg(test)]
use sandbox_common::{apply_create as shared_apply_create, apply_replace as shared_apply_replace};
use std::time::Duration;

use crate::config::EditToolConfig;
use crate::error::FrameworkError;
use crate::sandbox::{RunWasmRequest, WasmSandbox};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::read::{host_path_to_guest_path, resolve_path_for_read};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditTool {
    config: EditToolConfig,
}

#[derive(Debug, Clone)]
pub struct EditPlan {
    pub args: EditArgs,
    pub host_path: std::path::PathBuf,
    pub guest_path: Option<String>,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit local files using JSON: {command,path,...}. Supports create/replace/insert/delete/append with optional dry_run."
    }

    fn input_schema_json(&self) -> &'static str {
        "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\",\"enum\":[\"create\",\"replace\",\"insert\",\"delete\",\"append\"]},\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"},\"old_text\":{\"type\":\"string\"},\"new_text\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\",\"minimum\":1},\"replace_all\":{\"type\":\"boolean\"},\"overwrite\":{\"type\":\"boolean\"},\"dry_run\":{\"type\":\"boolean\"}},\"required\":[\"command\",\"path\"]}"
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

impl EditTool {
    pub fn plan(&self, ctx: &ToolExecEnv, args_json: &str) -> Result<EditPlan, FrameworkError> {
        let args: EditArgs = serde_json::from_str(args_json)
            .map_err(|e| FrameworkError::Tool(format!("edit requires JSON object args: {e}")))?;
        let host_path = resolve_path_for_read(&args.path, &ctx.workspace_root)?;
        let guest_path =
            host_path_to_guest_path(&host_path, &ctx.workspace_root, &ctx.persona_root).ok();
        Ok(EditPlan {
            args,
            host_path,
            guest_path,
        })
    }

    pub async fn execute_direct(
        &self,
        _ctx: &ToolExecEnv,
        plan: EditPlan,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let output = apply_edit_command_at_path(&plan.args, &plan.host_path)
            .map_err(|e| FrameworkError::Tool(e.to_string()))?;
        Ok(ToolRunOutput::plain(output))
    }

    pub async fn execute_wasm(
        &self,
        ctx: &ToolExecEnv,
        plan: EditPlan,
        runtime: &dyn WasmSandbox,
    ) -> Result<ToolRunOutput, FrameworkError> {
        let guest_path = plan.guest_path.ok_or_else(|| {
            FrameworkError::Tool("edit path is not representable inside wasm sandbox".to_owned())
        })?;
        let mut guest_args = plan.args.clone();
        guest_args.path = guest_path;
        let stdin = serde_json::to_vec(&guest_args)
            .map_err(|e| FrameworkError::Tool(format!("failed to serialize edit args: {e}")))?;
        let output = runtime
            .run(RunWasmRequest {
                workspace_root: ctx.workspace_root.clone(),
                persona_root: ctx.persona_root.clone(),
                artifact_name: "edit_tool.wasm",
                args: Vec::new(),
                stdin,
                timeout: Duration::from_secs(self.config.timeout_seconds.unwrap_or(15)),
            })
            .await?;
        if output.exit_code != 0 {
            return Err(FrameworkError::Tool(format!(
                "edit tool failed: exit_code={} stderr={}",
                output.exit_code,
                output.stderr.trim()
            )));
        }
        Ok(ToolRunOutput::plain(output.stdout))
    }
}

#[cfg(test)]
fn apply_create(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    shared_apply_create(path, path_display, args).map_err(FrameworkError::Tool)
}

#[cfg(test)]
fn apply_replace(
    path: &std::path::Path,
    path_display: &str,
    args: &EditArgs,
) -> Result<String, FrameworkError> {
    shared_apply_replace(path, path_display, args).map_err(FrameworkError::Tool)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};

    use super::{EditArgs, apply_create, apply_replace, resolve_path_for_read};

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_edit_{prefix}_{nanos}"))
    }

    fn args(value: Value) -> EditArgs {
        serde_json::from_value(value).expect("args should parse")
    }

    #[test]
    fn create_and_replace_roundtrip() {
        let workspace = unique_test_dir("create_replace");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");

        apply_create(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "create",
                "path": "notes.txt",
                "content": "hello world\n"
            })),
        )
        .expect("create should work");
        apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "notes.txt",
                "old_text": "world",
                "new_text": "team"
            })),
        )
        .expect("replace should work");

        let content = fs::read_to_string(path).expect("should read notes");
        assert_eq!(content, "hello team\n");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn replace_requires_replace_all_when_ambiguous() {
        let workspace = unique_test_dir("replace_ambiguous");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("notes.txt");
        fs::write(&path, "a b a b\n").expect("should write notes");

        let err = apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "notes.txt",
                "old_text": "a",
                "new_text": "z"
            })),
        )
        .expect_err("replace should fail without replace_all");
        assert!(err.to_string().contains("set replace_all=true"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn dry_run_does_not_mutate_file() {
        let workspace = unique_test_dir("dry_run");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("doc.txt");
        fs::write(&path, "alpha beta\n").expect("should write doc");

        let out = apply_replace(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "replace",
                "path": "doc.txt",
                "old_text": "beta",
                "new_text": "gamma",
                "dry_run": true
            })),
        )
        .expect("dry run should succeed");
        let parsed: Value = serde_json::from_str(&out).expect("output should be json");
        assert_eq!(parsed["status"], "dry_run");
        let content = fs::read_to_string(path).expect("should read doc");
        assert_eq!(content, "alpha beta\n");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn resolve_path_normalizes_outside_workspace_path() {
        let workspace = unique_test_dir("sandbox_workspace");
        let outside = unique_test_dir("sandbox_outside");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(&outside).expect("should create outside dir");
        fs::write(outside.join("secrets.txt"), "secret").expect("should write secret");

        let resolved = resolve_path_for_read(
            outside.join("secrets.txt").to_string_lossy().as_ref(),
            &workspace,
        )
        .expect("outside path should normalize");
        assert_eq!(resolved, outside.join("secrets.txt"));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn create_dry_run_does_not_write_file() {
        let workspace = unique_test_dir("create_dry_run");
        fs::create_dir_all(&workspace).expect("should create workspace");
        let path = workspace.join("draft.txt");

        let out = apply_create(
            &path,
            &path.display().to_string(),
            &args(json!({
                "command": "create",
                "path": "draft.txt",
                "content": "hello",
                "dry_run": true
            })),
        )
        .expect("create dry run should succeed");

        let parsed: Value = serde_json::from_str(&out).expect("output should be json");
        assert_eq!(parsed["status"], "dry_run");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn resolve_path_normalizes_persona_simpleclaw_path() {
        let workspace = unique_test_dir("persona_workspace");
        let persona = unique_test_dir("persona_root");
        fs::create_dir_all(&workspace).expect("should create workspace");
        fs::create_dir_all(persona.join(".simpleclaw")).expect("should create persona state");

        let resolved = resolve_path_for_read(
            persona
                .join(".simpleclaw/secret.db")
                .to_string_lossy()
                .as_ref(),
            &workspace,
        )
        .expect("persona path should normalize");
        assert_eq!(resolved, persona.join(".simpleclaw/secret.db"));

        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(persona);
    }
}
