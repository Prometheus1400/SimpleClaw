mod clock;
mod common;
mod edit;
pub(crate) mod exec;
mod forget;
mod memorize;
mod memory;
mod process;
mod read;
mod summon;
mod task;
mod web_fetch;
mod web_search;

use crate::tools::ToolRegistry;

pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    registry.register(memory::MemoryTool::SemanticQuery);
    registry.register(memorize::MemorizeTool::LongTermStore);
    registry.register(forget::ForgetTool::LongTermSemanticPrune);
    registry.register(summon::SummonTool::Handoff);
    registry.register(task::TaskTool::Worker);
    registry.register(web_search::WebSearchTool::DuckDuckGo);
    registry.register(clock::ClockTool::UtcNow);
    registry.register(web_fetch::WebFetchTool::HttpFetch);
    registry.register(read::ReadTool::LocalFile);
    registry.register(edit::EditTool::LocalFileEditor);
    registry.register(exec::ExecTool::ShellCommand);
    registry.register(process::ProcessTool::Lifecycle);
}
