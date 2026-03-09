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

use crate::tools::Tool;

pub(crate) fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(memory::MemoryTool::SemanticQuery),
        Box::new(memorize::MemorizeTool::LongTermStore),
        Box::new(forget::ForgetTool::LongTermSemanticPrune),
        Box::new(summon::SummonTool::Handoff),
        Box::new(task::TaskTool::Worker),
        Box::new(web_search::WebSearchTool::DuckDuckGo),
        Box::new(clock::ClockTool::UtcNow),
        Box::new(web_fetch::WebFetchTool::HttpFetch),
        Box::new(read::ReadTool::default()),
        Box::new(edit::EditTool::default()),
        Box::new(exec::ExecTool::default()),
        Box::new(process::ProcessTool::Lifecycle),
    ]
}
