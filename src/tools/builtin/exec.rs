use async_trait::async_trait;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::ExecToolConfig;
use crate::error::{FrameworkError, SandboxCapability};
use crate::sandbox::{HostSandbox, RunHostCommandRequest, SandboxPolicy, SpawnHostCommandRequest};
use crate::tools::{Tool, ToolExecEnv, ToolExecutionKind, ToolExecutionOutcome, ToolRunOutput};

use super::common::{command_output_to_json, exec_shell_command, parse_exec_args};
use super::file_access::resolve_path_for_read;

const DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS: u64 = 120;
const EXEC_DESCRIPTION_WITH_BG: &str =
    "Run local shell commands using JSON: {command, workdir?, background?}. Returns JSON string.";
const EXEC_DESCRIPTION_SYNC_ONLY: &str =
    "Run local shell commands using JSON: {command, workdir?}. Returns JSON string.";
const EXEC_SCHEMA_WITH_BG: &str = "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"command\"]}";
const EXEC_SCHEMA_SYNC_ONLY: &str = "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"}},\"required\":[\"command\"]}";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecTool {
    config: ExecToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPlan {
    pub command: String,
    pub background: bool,
    pub timeout_seconds: u64,
    pub env: std::collections::BTreeMap<String, String>,
    pub workdir: std::path::PathBuf,
    pub route: ExecToolRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecToolRoute {
    Sandboxed,
    NeedsApproval {
        capability: SandboxCapability,
        target: String,
        reason: String,
    },
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn description(&self) -> &'static str {
        if self.config.allow_background {
            EXEC_DESCRIPTION_WITH_BG
        } else {
            EXEC_DESCRIPTION_SYNC_ONLY
        }
    }

    fn input_schema_json(&self) -> &'static str {
        if self.config.allow_background {
            EXEC_SCHEMA_WITH_BG
        } else {
            EXEC_SCHEMA_SYNC_ONLY
        }
    }

    fn supported_execution_kinds(&self) -> &'static [ToolExecutionKind] {
        &[ToolExecutionKind::Direct, ToolExecutionKind::HostSandbox]
    }

    fn configure(&mut self, config: serde_json::Value) -> Result<(), FrameworkError> {
        self.config = serde_json::from_value(config)
            .map_err(|e| FrameworkError::Config(format!("tools.exec config is invalid: {e}")))?;
        Ok(())
    }

    async fn execute(
        &self,
        ctx: &ToolExecEnv<'_>,
        args_json: &str,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        let plan = self.plan(ctx, args_json)?;
        self.execute_direct(ctx, plan, session_id).await
    }
}

impl ExecTool {
    pub(crate) fn set_allow_background(&mut self, allow_background: bool) {
        self.config.allow_background = allow_background;
    }

    fn sandbox_policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            network_enabled: self.config.sandbox.network_enabled.unwrap_or(false),
            extra_writable_paths: self.config.sandbox.extra_writable_paths.clone(),
        }
    }

    pub fn plan(&self, ctx: &ToolExecEnv<'_>, args_json: &str) -> Result<ExecPlan, FrameworkError> {
        let args = parse_exec_args(args_json);
        if args.command.trim().is_empty() {
            return Err(FrameworkError::Tool(
                "exec requires a non-empty command".to_owned(),
            ));
        }
        if args.background && !self.config.allow_background {
            return Err(FrameworkError::Tool(
                "exec background mode is disabled by tools.exec.allow_background".to_owned(),
            ));
        }
        if args.background && !ctx.allow_async_tools {
            return Err(FrameworkError::Tool(
                "background async tools are not allowed in delegated runs".to_owned(),
            ));
        }
        let workdir = if let Some(workdir) = args.workdir.as_deref() {
            resolve_path_for_read(workdir, &ctx.workspace_root)?
        } else {
            ctx.workspace_root.to_path_buf()
        };
        if !workdir.is_dir() {
            return Err(FrameworkError::Tool(format!(
                "exec workdir must be a directory: {}",
                workdir.display()
            )));
        }
        Ok(ExecPlan {
            command: args.command.trim().to_owned(),
            background: args.background,
            timeout_seconds: self.config.timeout_seconds.unwrap_or(20),
            env: ctx.env.clone(),
            route: classify_exec_command_access(args.command.trim(), &workdir, &self.config)?,
            workdir,
        })
    }

    pub async fn execute_direct(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: ExecPlan,
        session_id: &str,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        if plan.background {
            let started = ctx
                .async_tool_runs
                .start_process(
                    "exec",
                    &plan.command,
                    &ctx.agent_id,
                    session_id,
                    Some(&plan.workdir),
                    &plan.env,
                    ctx.completion_tx.cloned(),
                    ctx.completion_route.cloned(),
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let result = exec_shell_command(
            &plan.command,
            Some(&plan.workdir),
            &plan.env,
            plan.timeout_seconds,
        )
        .await?;
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput::plain(
            result.to_string(),
        )))
    }

    pub async fn execute_host_sandboxed(
        &self,
        ctx: &ToolExecEnv<'_>,
        plan: ExecPlan,
        session_id: &str,
        runtime: &dyn HostSandbox,
    ) -> Result<ToolExecutionOutcome, FrameworkError> {
        if plan.background {
            let prepared = runtime
                .prepare_spawn(SpawnHostCommandRequest {
                    command: plan.command.clone(),
                    workspace_root: plan.workdir.clone(),
                    policy: self.sandbox_policy(),
                })
                .await?;
            let started = ctx
                .async_tool_runs
                .start_prepared_process(
                    "exec",
                    &plan.command,
                    &ctx.agent_id,
                    session_id,
                    prepared,
                    &plan.env,
                    ctx.completion_tx.cloned(),
                    ctx.completion_route.cloned(),
                )
                .await?;
            return Ok(ToolExecutionOutcome::AsyncStarted(started));
        }

        let command = plan.command.clone();
        let output = match runtime
            .run(RunHostCommandRequest {
                command,
                workspace_root: plan.workdir,
                policy: self.sandbox_policy(),
                env: plan.env,
                timeout_seconds: self
                    .config
                    .timeout_seconds
                    .unwrap_or(DEFAULT_SANDBOX_EXEC_TIMEOUT_SECS),
            })
            .await
        {
            Ok(output) => output,
            Err(err) => return Err(err),
        };
        Ok(ToolExecutionOutcome::Completed(ToolRunOutput::plain(
            command_output_to_json(output.exit_code, &output.stdout, &output.stderr).to_string(),
        )))
    }
}

fn classify_exec_command_access(
    command: &str,
    workdir: &Path,
    config: &ExecToolConfig,
) -> Result<ExecToolRoute, FrameworkError> {
    if command_uses_ambiguous_shell_features(command) {
        return Ok(ExecToolRoute::NeedsApproval {
            capability: SandboxCapability::Exec,
            target: command.to_owned(),
            reason: "command uses shell features that preflight cannot verify safely".to_owned(),
        });
    }

    let tokens = tokenize_shell_command(command).unwrap_or_else(|_| Vec::new());
    if tokens.is_empty() {
        return Ok(ExecToolRoute::NeedsApproval {
            capability: SandboxCapability::Exec,
            target: command.to_owned(),
            reason: "command could not be parsed safely during exec preflight".to_owned(),
        });
    }

    if !config.sandbox.network_enabled.unwrap_or(false)
        && tokens
            .iter()
            .any(|token| token.kind == ShellTokenKind::Word && is_network_tool(token.text.as_str()))
    {
        return Ok(ExecToolRoute::NeedsApproval {
            capability: SandboxCapability::Network,
            target: command.to_owned(),
            reason:
                "command appears to use network tooling outside the default exec sandbox policy"
                    .to_owned(),
        });
    }

    let allowed_roots = allowed_write_roots(workdir, config)?;
    let mut segments = split_shell_segments(&tokens);
    for segment in &mut segments {
        if let Some(route) = classify_segment_write_access(segment, workdir, &allowed_roots)? {
            return Ok(route);
        }
    }

    Ok(ExecToolRoute::Sandboxed)
}

fn classify_segment_write_access(
    tokens: &[ShellToken],
    workdir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<Option<ExecToolRoute>, FrameworkError> {
    for index in 0..tokens.len() {
        if !is_redirection_operator(tokens[index].text.as_str()) {
            continue;
        }
        let Some(target) = next_word(tokens, index + 1) else {
            return Ok(Some(ExecToolRoute::NeedsApproval {
                capability: SandboxCapability::Write,
                target: tokens[index].text.clone(),
                reason: "shell redirection target could not be resolved safely".to_owned(),
            }));
        };
        if let Some(route) = classify_write_target(target, workdir, allowed_roots)? {
            return Ok(Some(route));
        }
    }

    let Some(command_name) = segment_command_name(tokens) else {
        return Ok(None);
    };

    if matches!(command_name, "chmod" | "chown" | "mount" | "umount") {
        return Ok(Some(ExecToolRoute::NeedsApproval {
            capability: SandboxCapability::Exec,
            target: command_name.to_owned(),
            reason: format!("{command_name} is treated as a high-risk exec operation"),
        }));
    }

    let path_indexes = write_target_indexes(command_name, tokens);
    for index in path_indexes {
        let Some(token) = tokens.get(index) else {
            continue;
        };
        if token.kind != ShellTokenKind::Word || token.text == "-" {
            continue;
        }
        if let Some(route) = classify_write_target(&token.text, workdir, allowed_roots)? {
            return Ok(Some(route));
        }
    }

    Ok(None)
}

fn classify_write_target(
    raw_target: &str,
    workdir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<Option<ExecToolRoute>, FrameworkError> {
    if raw_target.contains('*') || raw_target.contains('?') || raw_target.contains('[') {
        return Ok(Some(ExecToolRoute::NeedsApproval {
            capability: SandboxCapability::Write,
            target: raw_target.to_owned(),
            reason: "write target uses glob expansion that preflight cannot verify safely"
                .to_owned(),
        }));
    }

    let resolved = resolve_exec_path(raw_target, workdir)?;
    if allowed_roots.iter().any(|root| resolved.starts_with(root)) {
        return Ok(None);
    }

    Ok(Some(ExecToolRoute::NeedsApproval {
        capability: SandboxCapability::Write,
        target: resolved.display().to_string(),
        reason: format!(
            "write target is outside the configured exec sandbox roots: {}",
            render_allowed_roots(allowed_roots)
        ),
    }))
}

fn allowed_write_roots(
    workdir: &Path,
    config: &ExecToolConfig,
) -> Result<Vec<PathBuf>, FrameworkError> {
    let mut roots = vec![
        canonicalize_existing_path(workdir)?,
        canonicalize_existing_path(Path::new("/tmp"))?,
    ];
    for root in &config.sandbox.extra_writable_paths {
        roots.push(resolve_exec_path(root, workdir)?);
    }
    Ok(roots)
}

fn render_allowed_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve_exec_path(raw_path: &str, workdir: &Path) -> Result<PathBuf, FrameworkError> {
    let normalized = resolve_path_for_read(raw_path, workdir)?;
    canonicalize_with_existing_ancestor(&normalized)
}

fn canonicalize_existing_path(path: &Path) -> Result<PathBuf, FrameworkError> {
    path.canonicalize().map_err(|e| {
        FrameworkError::Tool(format!("failed to canonicalize {}: {e}", path.display()))
    })
}

fn canonicalize_with_existing_ancestor(path: &Path) -> Result<PathBuf, FrameworkError> {
    if path.exists() {
        return canonicalize_existing_path(path);
    }

    let mut suffix = Vec::new();
    let mut current = path;
    while !current.exists() {
        let Some(name) = current.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }

    let mut resolved = canonicalize_existing_path(current)?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn command_uses_ambiguous_shell_features(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    lowered.contains("eval ")
        || lowered.starts_with("eval\t")
        || lowered.contains("`")
        || lowered.contains("$(")
        || lowered.contains("<(")
        || lowered.contains(">(")
        || lowered.contains("<<")
        || dynamic_shell_c_regex().is_match(command)
}

fn dynamic_shell_c_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?:^|[;&|]\s*)(?:bash|sh|zsh)\s+-c\s+([\"']).*\$[A-Za-z_{]"#)
            .expect("dynamic shell regex should compile")
    })
}

fn is_network_tool(command_name: &str) -> bool {
    matches!(
        command_name,
        "curl"
            | "wget"
            | "nc"
            | "ncat"
            | "netcat"
            | "ssh"
            | "scp"
            | "rsync"
            | "ftp"
            | "sftp"
            | "dig"
            | "nslookup"
            | "ping"
    )
}

fn write_target_indexes(command_name: &str, tokens: &[ShellToken]) -> Vec<usize> {
    let mut indexes = Vec::new();
    let Some(command_index) = tokens
        .iter()
        .position(|token| token.kind == ShellTokenKind::Word && token.text == command_name)
    else {
        return indexes;
    };

    match command_name {
        "touch" | "mkdir" | "rm" | "rmdir" | "truncate" => {
            indexes.extend(non_flag_word_indexes(tokens, command_index + 1));
        }
        "cp" | "mv" | "install" => {
            let words = non_flag_word_indexes(tokens, command_index + 1);
            if let Some(last) = words.last() {
                indexes.push(*last);
            }
        }
        "ln" => {
            let words = non_flag_word_indexes(tokens, command_index + 1);
            if words.len() >= 2 {
                indexes.push(*words.last().expect("len checked"));
            }
        }
        "tee" => {
            indexes.extend(non_flag_word_indexes(tokens, command_index + 1));
        }
        _ => {}
    }

    indexes
}

fn non_flag_word_indexes(tokens: &[ShellToken], start: usize) -> Vec<usize> {
    tokens
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, token)| token.kind == ShellTokenKind::Word)
        .filter(|(_, token)| !token.text.starts_with('-'))
        .map(|(index, _)| index)
        .collect()
}

fn segment_command_name(tokens: &[ShellToken]) -> Option<&str> {
    for token in tokens {
        if token.kind != ShellTokenKind::Word {
            continue;
        }
        if is_env_assignment(token.text.as_str()) {
            continue;
        }
        return Some(token.text.as_str());
    }
    None
}

fn is_env_assignment(token: &str) -> bool {
    token.contains('=') && !token.starts_with('=')
}

fn next_word(tokens: &[ShellToken], start: usize) -> Option<&str> {
    tokens
        .iter()
        .skip(start)
        .find(|token| token.kind == ShellTokenKind::Word)
        .map(|token| token.text.as_str())
}

fn split_shell_segments(tokens: &[ShellToken]) -> Vec<&[ShellToken]> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        if is_command_separator(token.text.as_str()) {
            if start < index {
                segments.push(&tokens[start..index]);
            }
            start = index + 1;
        }
    }
    if start < tokens.len() {
        segments.push(&tokens[start..]);
    }
    segments
}

fn is_command_separator(token: &str) -> bool {
    matches!(token, "&&" | "||" | ";" | "|")
}

fn is_redirection_operator(token: &str) -> bool {
    matches!(token, ">" | ">>" | "1>" | "1>>" | "2>" | "2>>")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellToken {
    kind: ShellTokenKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellTokenKind {
    Word,
    Operator,
}

fn tokenize_shell_command(command: &str) -> Result<Vec<ShellToken>, ()> {
    let chars: Vec<char> = command.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut index = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while index < chars.len() {
        let ch = chars[index];
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            index += 1;
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' {
                index += 1;
                if let Some(next) = chars.get(index) {
                    current.push(*next);
                }
            } else {
                current.push(ch);
            }
            index += 1;
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                index += 1;
            }
            '"' => {
                in_double = true;
                index += 1;
            }
            '\\' => {
                index += 1;
                if let Some(next) = chars.get(index) {
                    current.push(*next);
                    index += 1;
                }
            }
            ' ' | '\t' | '\n' => {
                push_word_token(&mut tokens, &mut current);
                index += 1;
            }
            '&' | '|' | ';' | '>' | '<' => {
                push_word_token(&mut tokens, &mut current);
                let operator = if index + 1 < chars.len() {
                    let next = chars[index + 1];
                    match (ch, next) {
                        ('&', '&') | ('|', '|') | ('>', '>') | ('<', '<') => {
                            index += 1;
                            format!("{ch}{next}")
                        }
                        _ => ch.to_string(),
                    }
                } else {
                    ch.to_string()
                };
                tokens.push(ShellToken {
                    kind: ShellTokenKind::Operator,
                    text: operator,
                });
                index += 1;
            }
            _ => {
                current.push(ch);
                index += 1;
            }
        }
    }

    if in_single || in_double {
        return Err(());
    }
    push_word_token(&mut tokens, &mut current);
    Ok(tokens)
}

fn push_word_token(tokens: &mut Vec<ShellToken>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    tokens.push(ShellToken {
        kind: ShellTokenKind::Word,
        text: std::mem::take(current),
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use serde_json::Value;

    use super::{ExecTool, ExecToolRoute, classify_exec_command_access};
    use crate::approval::UnavailableApprovalRequester;
    use crate::config::{DatabaseConfig, ExecToolConfig};
    use crate::error::{FrameworkError, SandboxCapability};
    use crate::memory::MemoryStore;
    use crate::tools::{
        AgentInvokeRequest, AgentInvoker, AsyncToolRunManager, InvokeOutcome, Tool, ToolExecEnv,
        ToolExecutionOutcome, WorkerInvokeRequest,
    };

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

    async fn test_ctx() -> ToolExecEnv<'static> {
        test_ctx_with_env(std::collections::BTreeMap::new()).await
    }

    async fn test_ctx_with_env(
        env_map: std::collections::BTreeMap<String, String>,
    ) -> ToolExecEnv<'static> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("simpleclaw_exec_test_{nanos}"));
        std::fs::create_dir_all(&root).expect("temp exec test dir should be created");
        let short = root.join("short.db");
        let long = root.join("long.db");
        let memory = MemoryStore::new_without_embedder(&short, &long, &DatabaseConfig::default())
            .await
            .expect("memory should initialize");
        let memory = Box::leak(Box::new(memory));
        let env = Box::leak(Box::new(env_map));
        let persona_root = Box::leak(Box::new(PathBuf::from(&root)));
        let workspace_root = Box::leak(Box::new(PathBuf::from(&root)));
        let owner_ids = Box::leak(Box::new(vec!["user-1".to_owned()]));
        let async_tool_runs = Box::leak(Box::new(Arc::new(AsyncToolRunManager::new())));
        let invoker: &'static Arc<dyn AgentInvoker> =
            Box::leak(Box::new(Arc::new(NoopInvoker) as Arc<dyn AgentInvoker>));
        ToolExecEnv {
            agent_id: "test-agent",
            agent_name: "Test Agent",
            memory,
            history_messages: 10,
            env,
            persona_root,
            workspace_root,
            user_id: "user-1",
            owner_ids,
            async_tool_runs,
            invoker,
            gateway: None,
            completion_tx: None,
            completion_route: None,
            allow_async_tools: true,
            approval_requester: Arc::new(UnavailableApprovalRequester),
        }
    }

    #[tokio::test]
    async fn exec_rejects_empty_command() {
        let tool = ExecTool::default();
        let ctx = test_ctx().await;

        let err = tool
            .execute(&ctx, r#"{"command":"   "}"#, "sess-1")
            .await
            .err()
            .expect("empty command should fail");

        assert!(
            err.to_string()
                .contains("exec requires a non-empty command")
        );
    }

    #[tokio::test]
    async fn exec_rejects_background_when_disabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": false,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let ctx = test_ctx().await;

        let err = tool
            .execute(
                &ctx,
                "{\"command\":\"sleep 1\",\"background\":true}",
                "sess-1",
            )
            .await
            .err()
            .expect("background execution should fail");

        assert!(
            err.to_string()
                .contains("exec background mode is disabled by tools.exec.allow_background")
        );
    }

    #[tokio::test]
    async fn exec_runs_foreground_command_without_sandbox() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(&ctx, r#"{"command":"printf hello"}"#, "sess-1")
            .await
            .expect("foreground exec should succeed");
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["exitCode"], 0);
        assert_eq!(parsed["stdout"], "hello");
        assert_eq!(parsed["stderr"], "");
    }

    #[tokio::test]
    async fn exec_injects_configured_env_into_foreground_command() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let ctx = test_ctx_with_env(std::collections::BTreeMap::from([(
            "SIMPLECLAW_EXEC_TEST_TOKEN".to_owned(),
            "from-config".to_owned(),
        )]))
        .await;

        let output = tool
            .execute(
                &ctx,
                r#"{"command":"printf %s \"$SIMPLECLAW_EXEC_TEST_TOKEN\""}"#,
                "sess-1",
            )
            .await
            .expect("foreground exec should succeed");
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

        assert_eq!(parsed["stdout"], "from-config");
    }

    #[tokio::test]
    async fn exec_runs_in_overridden_workdir() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({ "sandbox": { "enabled": false } }))
            .expect("config should apply");
        let ctx = test_ctx().await;
        let nested = ctx.workspace_root.join("nested");
        std::fs::create_dir_all(&nested).expect("nested directory should exist");

        let output = tool
            .execute(
                &ctx,
                &serde_json::json!({
                    "command": "pwd",
                    "workdir": nested.to_string_lossy(),
                })
                .to_string(),
                "sess-1",
            )
            .await
            .expect("foreground exec should succeed");
        let ToolExecutionOutcome::Completed(output) = output else {
            panic!("foreground exec should complete immediately");
        };
        let parsed: Value =
            serde_json::from_str(&output.output).expect("exec output should be json");

        let stdout = parsed["stdout"].as_str().expect("stdout should be string");
        assert!(stdout.ends_with("/nested"));
        assert!(stdout.contains("simpleclaw_exec_test_"));
    }

    #[tokio::test]
    async fn exec_backgrounds_process_when_enabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let ctx = test_ctx().await;

        let output = tool
            .execute(
                &ctx,
                r#"{"command":"sleep 0.1","background":true}"#,
                "sess-1",
            )
            .await
            .expect("background exec should succeed");
        let ToolExecutionOutcome::AsyncStarted(output) = output else {
            panic!("background exec should start async tool run");
        };
        let parsed: Value =
            serde_json::from_str(&output.accepted_output()).expect("exec output should be json");

        assert_eq!(parsed["status"], "accepted");
        let run_id = parsed["runId"]
            .as_str()
            .expect("accepted response should include run id");
        let sessions = ctx
            .async_tool_runs
            .list_for_session(&ctx.agent_id, "sess-1")
            .await;
        assert!(sessions.iter().any(|snapshot| snapshot.run_id == run_id));
    }

    #[tokio::test]
    async fn exec_rejects_background_when_async_tools_disallowed_in_context() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let mut ctx = test_ctx().await;
        ctx.allow_async_tools = false;

        let err = tool
            .execute(
                &ctx,
                r#"{"command":"sleep 0.1","background":true}"#,
                "sess-1",
            )
            .await
            .err()
            .expect("background execution should fail");

        assert!(
            err.to_string()
                .contains("background async tools are not allowed in delegated runs")
        );
    }

    #[test]
    fn exec_schema_hides_background_when_disabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": false,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");

        assert_eq!(
            tool.input_schema_json(),
            "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"}},\"required\":[\"command\"]}"
        );
        assert_eq!(
            tool.description(),
            "Run local shell commands using JSON: {command, workdir?}. Returns JSON string."
        );
    }

    #[test]
    fn exec_schema_exposes_background_when_enabled() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");

        assert_eq!(
            tool.input_schema_json(),
            "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"workdir\":{\"type\":\"string\"},\"background\":{\"type\":\"boolean\"}},\"required\":[\"command\"]}"
        );
    }

    #[test]
    fn exec_preflight_marks_workspace_redirection_as_sandboxed() {
        let config = ExecToolConfig::default();
        let root = std::env::temp_dir().join(format!(
            "simpleclaw_exec_route_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("temp root should be created");

        let route = classify_exec_command_access("printf hi > nested.txt", &root, &config)
            .expect("preflight should succeed");

        assert_eq!(route, ExecToolRoute::Sandboxed);
    }

    #[test]
    fn exec_preflight_requests_approval_for_outside_write_target() {
        let config = ExecToolConfig::default();
        let root = std::env::temp_dir().join(format!(
            "simpleclaw_exec_route_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("temp root should be created");

        let route =
            classify_exec_command_access("printf hi > /Users/shared/blocked.txt", &root, &config)
                .expect("preflight should succeed");

        assert!(matches!(
            route,
            ExecToolRoute::NeedsApproval {
                capability: SandboxCapability::Write,
                ..
            }
        ));
    }

    #[test]
    fn exec_preflight_requests_approval_for_dynamic_shell() {
        let config = ExecToolConfig::default();
        let root = std::env::temp_dir().join(format!(
            "simpleclaw_exec_route_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("temp root should be created");

        let route = classify_exec_command_access("bash -c \"$PAYLOAD\"", &root, &config)
            .expect("preflight should succeed");

        assert!(matches!(
            route,
            ExecToolRoute::NeedsApproval {
                capability: SandboxCapability::Exec,
                ..
            }
        ));
    }

    #[test]
    fn exec_preflight_requests_approval_for_network_tools_when_disabled() {
        let config = ExecToolConfig::default();
        let root = std::env::temp_dir().join(format!(
            "simpleclaw_exec_route_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("temp root should be created");

        let route = classify_exec_command_access("curl https://example.com", &root, &config)
            .expect("preflight should succeed");

        assert!(matches!(
            route,
            ExecToolRoute::NeedsApproval {
                capability: SandboxCapability::Network,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn exec_injects_configured_env_into_background_command() {
        let mut tool = ExecTool::default();
        tool.configure(serde_json::json!({
            "allow_background": true,
            "sandbox": { "enabled": false }
        }))
        .expect("config should apply");
        let ctx = test_ctx_with_env(std::collections::BTreeMap::from([(
            "SIMPLECLAW_EXEC_BG_TOKEN".to_owned(),
            "background-value".to_owned(),
        )]))
        .await;
        let output_path = ctx.workspace_root.join("bg-env.txt");

        let command = format!(
            "printf %s \"$SIMPLECLAW_EXEC_BG_TOKEN\" > {}",
            output_path.display()
        );
        tool.execute(
            &ctx,
            &serde_json::json!({ "command": command, "background": true }).to_string(),
            "sess-1",
        )
        .await
        .expect("background exec should succeed");

        for _ in 0..20 {
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                assert_eq!(content, "background-value");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        panic!("background exec did not write env output");
    }
}
