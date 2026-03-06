use std::path::{Path, PathBuf};

use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::preview1::{self, WasiP1Ctx};

use crate::config::SandboxMode;
use crate::error::FrameworkError;

use super::{Tool, ToolCtx};

const WASM_NOOP_START_MODULE: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
    0x01, 0x00, 0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, 0x00, 0x00, 0x0a, 0x04,
    0x01, 0x02, 0x00, 0x0b,
];

pub async fn execute_tool_with_sandbox(
    tool: &dyn Tool,
    ctx: &ToolCtx,
    args_json: &str,
    session_id: &str,
) -> Result<String, FrameworkError> {
    match ctx.sandbox {
        SandboxMode::Off => tool.execute(ctx, args_json, session_id).await,
        SandboxMode::Wasm => {
            bootstrap_wasm_workspace_sandbox(&ctx.workspace_root)?;
            tool.execute(ctx, args_json, session_id).await
        }
    }
}

fn bootstrap_wasm_workspace_sandbox(workspace_root: &Path) -> Result<(), FrameworkError> {
    let workspace = normalize_workspace_root(workspace_root)?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .preopened_dir(
            &workspace,
            "/workspace",
            wasmtime_wasi::DirPerms::all(),
            wasmtime_wasi::FilePerms::all(),
        )
        .map_err(|e| {
            FrameworkError::Tool(format!(
                "wasm sandbox failed to preopen workspace: path={} error={e}",
                workspace.display()
            ))
        })?;
    let wasi_ctx = wasi_builder.build_p1();

    let engine = Engine::default();
    let module = Module::new(&engine, WASM_NOOP_START_MODULE)
        .map_err(|e| FrameworkError::Tool(format!("wasm sandbox failed to load module: {e}")))?;
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |wasi: &mut WasiP1Ctx| wasi)
        .map_err(|e| FrameworkError::Tool(format!("wasm sandbox failed to link wasi: {e}")))?;
    let mut store: Store<WasiP1Ctx> = Store::new(&engine, wasi_ctx);

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| FrameworkError::Tool(format!("wasm sandbox failed to instantiate: {e}")))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| FrameworkError::Tool(format!("wasm sandbox missing _start function: {e}")))?;
    start
        .call(&mut store, ())
        .map_err(|e| FrameworkError::Tool(format!("wasm sandbox failed to run _start: {e}")))?;
    Ok(())
}

fn normalize_workspace_root(workspace_root: &Path) -> Result<PathBuf, FrameworkError> {
    if workspace_root.is_absolute() {
        return Ok(workspace_root.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(FrameworkError::Io)?;
    Ok(cwd.join(workspace_root))
}
