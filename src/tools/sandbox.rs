use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::task::spawn_blocking;
use tokio::time::timeout;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

use crate::config::SandboxMode;
use crate::error::FrameworkError;

use super::{Tool, ToolCtx};

const WASM_STDIO_CAPACITY: usize = 2 * 1024 * 1024;
const WASM_WORKSPACE_MOUNT: &str = "/workspace";
const WASM_TMP_MOUNT: &str = "/tmp";

pub struct WasmGuestOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn execute_tool_with_sandbox(
    tool: &dyn Tool,
    ctx: &ToolCtx,
    args_json: &str,
    session_id: &str,
) -> Result<String, FrameworkError> {
    match ctx.sandbox {
        SandboxMode::Off | SandboxMode::On => tool.execute(ctx, args_json, session_id).await,
    }
}

pub async fn run_wasm_guest(
    workspace_root: &Path,
    artifact_name: &str,
    args: &[String],
    stdin: &[u8],
    time_limit: Duration,
) -> Result<WasmGuestOutput, FrameworkError> {
    let workspace = normalize_workspace_root(workspace_root)?;
    let tmp_dir = env::temp_dir();
    let artifact_path = resolve_guest_artifact_path(artifact_name, &workspace)?;
    let args = args.to_vec();
    let stdin = stdin.to_vec();

    let join = spawn_blocking(move || {
        run_wasm_guest_blocking(&workspace, &tmp_dir, &artifact_path, &args, &stdin)
    });

    let joined = timeout(time_limit, join)
        .await
        .map_err(|_| FrameworkError::Tool(format!("wasm guest timed out after {time_limit:?}")))?;
    joined.map_err(|e| FrameworkError::Tool(format!("wasm guest failed to join: {e}")))?
}

fn run_wasm_guest_blocking(
    workspace_root: &Path,
    tmp_dir: &Path,
    artifact_path: &Path,
    args: &[String],
    stdin: &[u8],
) -> Result<WasmGuestOutput, FrameworkError> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, artifact_path).map_err(|e| {
        FrameworkError::Tool(format!(
            "wasm guest failed to load module: module={} error={e}",
            artifact_path.display()
        ))
    })?;
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |wasi: &mut WasiP1Ctx| wasi)
        .map_err(|e| FrameworkError::Tool(format!("wasm guest failed to link wasi: {e}")))?;

    let mut wasi_builder = WasiCtxBuilder::new();
    let stdin_pipe = MemoryInputPipe::new(stdin.to_vec());
    let stdout_pipe = MemoryOutputPipe::new(WASM_STDIO_CAPACITY);
    let stderr_pipe = MemoryOutputPipe::new(WASM_STDIO_CAPACITY);
    wasi_builder.stdin(stdin_pipe);
    wasi_builder.stdout(stdout_pipe.clone());
    wasi_builder.stderr(stderr_pipe.clone());
    wasi_builder.arg(
        artifact_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("guest"),
    );
    for arg in args {
        wasi_builder.arg(arg);
    }
    wasi_builder
        .preopened_dir(
            workspace_root,
            WASM_WORKSPACE_MOUNT,
            DirPerms::all(),
            FilePerms::all(),
        )
        .map_err(|e| {
            FrameworkError::Tool(format!(
                "wasm guest failed to preopen workspace: path={} error={e}",
                workspace_root.display()
            ))
        })?;
    wasi_builder
        .preopened_dir(tmp_dir, WASM_TMP_MOUNT, DirPerms::all(), FilePerms::all())
        .map_err(|e| {
            FrameworkError::Tool(format!(
                "wasm guest failed to preopen tmp dir: path={} error={e}",
                tmp_dir.display()
            ))
        })?;
    let wasi_ctx = wasi_builder.build_p1();
    let mut store: Store<WasiP1Ctx> = Store::new(&engine, wasi_ctx);

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| FrameworkError::Tool(format!("wasm guest failed to instantiate: {e}")))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| FrameworkError::Tool(format!("wasm guest missing _start function: {e}")))?;
    let exit_code = match start.call(&mut store, ()) {
        Ok(()) => 0,
        Err(err) => {
            if let Some(exit) = err.downcast_ref::<I32Exit>() {
                exit.0
            } else {
                return Err(FrameworkError::Tool(format!(
                    "wasm guest failed during _start: module={} error={err}",
                    artifact_path.display()
                )));
            }
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_pipe.contents()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_pipe.contents()).into_owned();
    Ok(WasmGuestOutput {
        stdout,
        stderr,
        exit_code,
    })
}

pub fn normalize_workspace_root(workspace_root: &Path) -> Result<PathBuf, FrameworkError> {
    if workspace_root.is_absolute() {
        return Ok(workspace_root.to_path_buf());
    }
    let cwd = env::current_dir().map_err(FrameworkError::Io)?;
    Ok(cwd.join(workspace_root))
}

pub fn workspace_guest_mount_path() -> &'static str {
    WASM_WORKSPACE_MOUNT
}

fn resolve_guest_artifact_path(
    artifact_name: &str,
    workspace_root: &Path,
) -> Result<PathBuf, FrameworkError> {
    let env_candidate = env::var_os("SIMPLECLAW_WASM_ASSETS_DIR")
        .map(PathBuf::from)
        .map(|root| root.join(artifact_name));
    let workspace_candidate = workspace_root.join("assets").join("wasm").join(artifact_name);
    let manifest_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("wasm")
        .join(artifact_name);

    let candidates = [env_candidate, Some(workspace_candidate), Some(manifest_candidate)];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(FrameworkError::Tool(format!(
        "missing wasm artifact: name={artifact_name} searched={} and {}",
        workspace_root.join("assets/wasm").display(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets/wasm")
            .display()
    )))
}

#[cfg(test)]
mod tests {
    use super::resolve_guest_artifact_path;
    use std::path::Path;

    #[test]
    fn resolve_guest_artifact_path_errors_for_missing_module() {
        let workspace = Path::new("/tmp/simpleclaw_missing_artifact");
        let err = resolve_guest_artifact_path("definitely_missing.wasm", workspace)
            .expect_err("missing artifact should return error");
        assert!(err.to_string().contains("missing wasm artifact"));
    }
}
