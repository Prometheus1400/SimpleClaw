mod execute;
mod finalize;
mod prepare;

use crate::agent::AgentDirectory;
use crate::approval::ApprovalRegistry;
use crate::channels::InboundMessage;
use crate::gateway::Gateway;
use crate::react::ReactLoop;
use crate::tools::AsyncToolRunManager;
pub(crate) use execute::TurnEngine;

pub(crate) struct TurnRuntime<'a> {
    pub gateway: &'a std::sync::Arc<Gateway>,
    pub directory: &'a AgentDirectory,
    pub react_loop: &'a ReactLoop,
    pub async_tool_runs: &'a std::sync::Arc<AsyncToolRunManager>,
    pub approval_registry: &'a std::sync::Arc<ApprovalRegistry>,
    pub completion_tx: &'a tokio::sync::mpsc::Sender<InboundMessage>,
}

pub(crate) struct TurnRequest<'a> {
    pub inbound: &'a InboundMessage,
    pub memory_session_id: &'a str,
    pub on_text_delta: Option<&'a (dyn Fn(&str) + Send + Sync)>,
    pub on_tool_status: Option<&'a (dyn Fn(Option<String>) + Send + Sync)>,
}

pub(crate) enum TurnDisposition {
    ContextRecorded,
    NoReply,
    Replied(TurnOutcome),
}

pub(crate) struct TurnOutcome {
    pub reply: String,
    pub tool_calls: Vec<crate::dispatch::ToolExecutionResult>,
    pub memory_recall_used: bool,
    pub memory_recall_short_hits: usize,
    pub memory_recall_long_hits: usize,
}
