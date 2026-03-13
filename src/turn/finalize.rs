use crate::error::FrameworkError;
use crate::memory::{Memory, StoredRole};
use crate::react::RunOutcome;
use crate::reply_policy::is_no_reply;

use super::{TurnDisposition, TurnOutcome};

pub(super) async fn finalize_turn(
    memory: &dyn Memory,
    session_id: &str,
    mut outcome: RunOutcome,
    memory_recall_short_hits: usize,
    memory_recall_long_hits: usize,
) -> Result<TurnDisposition, FrameworkError> {
    outcome.memory_recall_used = memory_recall_short_hits + memory_recall_long_hits > 0;
    outcome.memory_recall_short_hits = memory_recall_short_hits;
    outcome.memory_recall_long_hits = memory_recall_long_hits;

    if is_no_reply(&outcome.reply) {
        return Ok(TurnDisposition::NoReply);
    }

    memory
        .append_message(session_id, StoredRole::Assistant, &outcome.reply, None)
        .await?;

    Ok(TurnDisposition::Replied(TurnOutcome {
        reply: outcome.reply,
        tool_calls: outcome.tool_calls,
        memory_recall_used: outcome.memory_recall_used,
        memory_recall_short_hits: outcome.memory_recall_short_hits,
        memory_recall_long_hits: outcome.memory_recall_long_hits,
    }))
}
