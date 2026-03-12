mod discord;
pub(crate) mod policy;
mod types;

use async_trait::async_trait;
use std::future::pending;

use crate::approval::PendingApprovalRequest;
use crate::error::FrameworkError;

pub use discord::DiscordChannel;
pub use types::{ApprovalResolution, ChannelInbound, InboundMessage};

#[async_trait]
pub trait Channel: Send + Sync {
    fn supports_message_editing(&self) -> bool {
        false
    }

    fn message_char_limit(&self) -> Option<usize> {
        None
    }

    fn supports_approval_resolution(&self) -> bool {
        false
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), FrameworkError>;
    async fn send_approval_request(
        &self,
        channel_id: &str,
        request: &PendingApprovalRequest,
    ) -> Result<(), FrameworkError> {
        self.send_message(channel_id, &format_approval_request(request))
            .await
    }
    async fn send_message_with_id(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Option<String>, FrameworkError> {
        self.send_message(channel_id, content).await?;
        Ok(None)
    }
    async fn edit_message(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _content: &str,
    ) -> Result<(), FrameworkError> {
        Err(FrameworkError::Tool(
            "channel does not support editing messages".to_owned(),
        ))
    }
    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), FrameworkError>;
    async fn broadcast_typing(&self, channel_id: &str) -> Result<(), FrameworkError>;
    async fn listen(&self) -> Result<ChannelInbound, FrameworkError>;
    async fn listen_for_approval(&self) -> Result<ApprovalResolution, FrameworkError> {
        pending::<Result<ApprovalResolution, FrameworkError>>().await
    }
}

fn format_approval_request(request: &PendingApprovalRequest) -> String {
    format!(
        "Approval required.\napproval_id: {}\nrequesting_user_id: {}\ntool: {}\nreason: {}\naction: {}\nexecution_kind: {}\ncapability: {}\ndiagnostic: {}\nOnly the requesting user may approve or deny this exact tool call outside the sandbox.",
        request.approval_id,
        request.requesting_user_id,
        request.tool_name,
        request.reason,
        request.action_summary,
        request.execution_kind,
        request.capability,
        request.diagnostic
    )
}
