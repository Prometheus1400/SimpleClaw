mod clock;
mod common;
pub(crate) mod cron;
mod edit;
pub(crate) mod exec;
mod forget;
mod memorize;
mod memory;
mod process;
mod react;
mod read;
mod summon;
mod task;
mod web_fetch;
mod web_search;

use crate::tools::Tool;

pub(crate) fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(memory::MemoryTool::default()),
        Box::new(memorize::MemorizeTool::LongTermStore),
        Box::new(forget::ForgetTool::LongTermSemanticPrune),
        Box::new(summon::SummonTool::default()),
        Box::new(task::TaskTool::default()),
        Box::new(web_search::WebSearchTool::default()),
        Box::new(clock::ClockTool::UtcNow),
        Box::new(react::ReactTool::default()),
        Box::new(web_fetch::WebFetchTool::default()),
        Box::new(read::ReadTool::default()),
        Box::new(edit::EditTool::default()),
        Box::new(exec::ExecTool::default()),
        Box::new(process::ProcessTool::Lifecycle),
    ]
}
