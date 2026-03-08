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

use std::sync::Arc;

use crate::tools::Tool;

pub(crate) fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(memory::MemoryTool::SemanticQuery),
        Arc::new(memorize::MemorizeTool::LongTermStore),
        Arc::new(forget::ForgetTool::LongTermSemanticPrune),
        Arc::new(summon::SummonTool::Handoff),
        Arc::new(task::TaskTool::Worker),
        Arc::new(web_search::WebSearchTool::DuckDuckGo),
        Arc::new(clock::ClockTool::UtcNow),
        Arc::new(web_fetch::WebFetchTool::HttpFetch),
        Arc::new(read::ReadTool::LocalFile),
        Arc::new(edit::EditTool::LocalFileEditor),
        Arc::new(exec::ExecTool::ShellCommand),
        Arc::new(process::ProcessTool::Lifecycle),
    ]
}
