use async_trait::async_trait;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::task::spawn_blocking;
use tokio::time::timeout;
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

use crate::error::FrameworkError;
use crate::sandbox::{RunWasmRequest, WasmRunResult, WasmSandbox};

const WASM_STDIO_CAPACITY: usize = 2 * 1024 * 1024;
const WASM_WORKSPACE_MOUNT: &str = "/workspace";
const WASM_PERSONA_MOUNT: &str = "/persona";
const WASM_TMP_MOUNT: &str = "/tmp";
static WASM_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) struct DefaultWasmSandbox;

#[async_trait]
impl WasmSandbox for DefaultWasmSandbox {
    async fn run(&self, request: RunWasmRequest) -> Result<WasmRunResult, FrameworkError> {
        run_wasm_guest(request).await
    }
}

async fn run_wasm_guest(request: RunWasmRequest) -> Result<WasmRunResult, FrameworkError> {
    let workspace = normalize_workspace_root(&request.workspace_root)?;
    let persona = normalize_workspace_root(&request.persona_root)?;
    let artifact_path = resolve_guest_artifact_path(request.artifact_name, &workspace)?;
    let args = request.args;
    let stdin = request.stdin;

    let join = spawn_blocking(move || {
        run_wasm_guest_blocking(&workspace, &persona, &artifact_path, &args, &stdin)
    });

    let joined = timeout(request.timeout, join).await.map_err(|_| {
        FrameworkError::Tool(format!("wasm guest timed out after {:?}", request.timeout))
    })?;
    joined.map_err(|e| FrameworkError::Tool(format!("wasm guest failed to join: {e}")))?
}

fn run_wasm_guest_blocking(
    workspace_root: &Path,
    persona_root: &Path,
    artifact_path: &Path,
    args: &[String],
    stdin: &[u8],
) -> Result<WasmRunResult, FrameworkError> {
    let isolated_tmp = IsolatedTmpDir::create()?;

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
        .preopened_dir(
            persona_root,
            WASM_PERSONA_MOUNT,
            DirPerms::all(),
            FilePerms::all(),
        )
        .map_err(|e| {
            FrameworkError::Tool(format!(
                "wasm guest failed to preopen persona: path={} error={e}",
                persona_root.display()
            ))
        })?;
    wasi_builder
        .preopened_dir(
            isolated_tmp.path(),
            WASM_TMP_MOUNT,
            DirPerms::all(),
            FilePerms::all(),
        )
        .map_err(|e| {
            FrameworkError::Tool(format!(
                "wasm guest failed to preopen tmp dir: path={} error={e}",
                isolated_tmp.path().display()
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
    Ok(WasmRunResult {
        stdout,
        stderr,
        exit_code,
    })
}

pub(crate) fn normalize_workspace_root(workspace_root: &Path) -> Result<PathBuf, FrameworkError> {
    if workspace_root.is_absolute() {
        return Ok(workspace_root.to_path_buf());
    }
    let cwd = env::current_dir().map_err(FrameworkError::Io)?;
    Ok(cwd.join(workspace_root))
}

pub(crate) fn workspace_guest_mount_path() -> &'static str {
    WASM_WORKSPACE_MOUNT
}

pub(crate) fn persona_guest_mount_path() -> &'static str {
    WASM_PERSONA_MOUNT
}

fn resolve_guest_artifact_path(
    artifact_name: &str,
    _workspace_root: &Path,
) -> Result<PathBuf, FrameworkError> {
    let env_candidate = env::var_os("SIMPLECLAW_WASM_ASSETS_DIR")
        .map(PathBuf::from)
        .map(|root| root.join(artifact_name));
    let exe_candidate = std::env::current_exe().ok().and_then(|exe| {
        exe.parent()
            .and_then(|bin_dir| bin_dir.parent())
            .map(|prefix| {
                prefix
                    .join("share")
                    .join("simpleclaw")
                    .join("wasm")
                    .join(artifact_name)
            })
    });
    let release_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(artifact_name);
    let debug_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("wasm32-wasip1")
        .join("debug")
        .join(artifact_name);

    let candidates = [
        ("SIMPLECLAW_WASM_ASSETS_DIR", env_candidate),
        ("installed prefix", exe_candidate),
        ("cargo target release", Some(release_candidate)),
        ("cargo target debug", Some(debug_candidate)),
    ];
    for candidate in candidates.iter().filter_map(|(_, c)| c.as_ref()) {
        if candidate.is_file() {
            return Ok(candidate.to_path_buf());
        }
    }

    let searched_paths = candidates
        .iter()
        .filter_map(|(_, candidate)| candidate.as_ref())
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(FrameworkError::Tool(format!(
        "missing wasm artifact: name={artifact_name} searched=[{searched_paths}] (build with: cargo build --package read_tool --package edit_tool --target wasm32-wasip1 --release)"
    )))
}

struct IsolatedTmpDir {
    path: PathBuf,
}

impl IsolatedTmpDir {
    fn create() -> Result<Self, FrameworkError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| FrameworkError::Tool(format!("system clock error: {e}")))?;
        let counter = WASM_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir_name = format!(
            "simpleclaw_wasm_{}_{}_{}",
            now.as_secs(),
            now.subsec_nanos(),
            counter
        );
        let path = env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&path).map_err(FrameworkError::Io)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IsolatedTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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
