mod clock;
mod common;
pub(crate) mod cron;
pub(crate) mod edit;
pub(crate) mod exec;
mod forget;
mod memorize;
mod memory;
mod process;
mod react;
pub(crate) mod read;
mod summon;
mod task;
mod web_fetch;
mod web_search;

use crate::tools::{RegisteredTool, Tool};

pub(crate) fn builtin_tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::Direct(
            std::sync::Arc::new(memory::MemoryTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(memorize::MemorizeTool::LongTermStore) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(forget::ForgetTool::LongTermSemanticPrune)
                as std::sync::Arc<dyn Tool>,
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(summon::SummonTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(task::TaskTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(web_search::WebSearchTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(clock::ClockTool::UtcNow) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(react::ReactTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Direct(
            std::sync::Arc::new(web_fetch::WebFetchTool::default()) as std::sync::Arc<dyn Tool>
        ),
        RegisteredTool::Read(std::sync::Arc::new(read::ReadTool::default())),
        RegisteredTool::Edit(std::sync::Arc::new(edit::EditTool::default())),
        RegisteredTool::Exec(std::sync::Arc::new(exec::ExecTool::default())),
        RegisteredTool::Direct(
            std::sync::Arc::new(process::ProcessTool::Lifecycle) as std::sync::Arc<dyn Tool>
        ),
    ]
}
