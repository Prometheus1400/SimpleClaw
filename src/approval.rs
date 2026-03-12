//! Session-scoped approval requesting for one-shot sandbox escalations.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::warn;

use crate::channels::InboundMessage;
use crate::error::{FrameworkError, SandboxCapability};
use crate::gateway::Gateway;

/// User decision for a pending sandbox escalation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Execute the blocked tool call outside the sandbox.
    Approved,
    /// Reject the blocked tool call.
    Denied,
    /// No decision arrived before the approval timeout elapsed.
    TimedOut,
}

/// A structured approval request emitted by the React/tool execution path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Agent currently executing the turn.
    pub agent_id: String,
    /// Human-facing agent name from config.
    pub agent_name: String,
    /// Session key for the running turn.
    pub session_id: String,
    /// User id that initiated the request and is allowed to resolve it.
    pub requesting_user_id: String,
    /// Tool name that triggered the request.
    pub tool_name: String,
    /// Sandbox execution mode that produced the denial.
    pub execution_kind: String,
    /// Capability blocked by the sandbox.
    pub capability: SandboxCapability,
    /// Human-readable reason shown to the user.
    pub reason: String,
    /// Exact action preview for the blocked tool call.
    pub action_summary: String,
    /// Original sandbox/runtime diagnostic text.
    pub diagnostic: String,
}

/// A pending approval request with a stable identifier for channel callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalRequest {
    /// Stable identifier used by the UI callback to resolve the request.
    pub approval_id: String,
    /// Agent currently executing the turn.
    pub agent_id: String,
    /// Human-facing agent name from config.
    pub agent_name: String,
    /// Session key for the running turn.
    pub session_id: String,
    /// User id that initiated the request and is allowed to resolve it.
    pub requesting_user_id: String,
    /// Tool name that triggered the request.
    pub tool_name: String,
    /// Sandbox execution mode that produced the denial.
    pub execution_kind: String,
    /// Capability blocked by the sandbox.
    pub capability: String,
    /// Human-readable reason shown to the user.
    pub reason: String,
    /// Exact action preview for the blocked tool call.
    pub action_summary: String,
    /// Original sandbox/runtime diagnostic text.
    pub diagnostic: String,
}

struct PendingApproval {
    request: PendingApprovalRequest,
    response_tx: Option<oneshot::Sender<ApprovalDecision>>,
}

/// Async approval callback exposed to the React/tool execution path.
#[async_trait]
pub trait ApprovalRequester: Send + Sync {
    /// Send a structured request to the user and await the decision.
    async fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, FrameworkError>;
}

/// Shared approval requester handle used by tool execution and delegated runs.
pub type DynApprovalRequester = Arc<dyn ApprovalRequester>;

/// Shared registry for pending approvals, resolved by gateway/channel callbacks.
#[derive(Default)]
pub struct ApprovalRegistry {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, PendingApproval>>,
}

impl ApprovalRegistry {
    /// Create an empty approval registry.
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Register a pending request and return the routed request plus receiver.
    pub async fn register(
        &self,
        request: ApprovalRequest,
    ) -> (PendingApprovalRequest, oneshot::Receiver<ApprovalDecision>) {
        let approval_id = format!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let routed = PendingApprovalRequest {
            approval_id: approval_id.clone(),
            agent_id: request.agent_id,
            agent_name: request.agent_name,
            session_id: request.session_id,
            requesting_user_id: request.requesting_user_id,
            tool_name: request.tool_name,
            execution_kind: request.execution_kind,
            capability: request.capability.as_str().to_owned(),
            reason: request.reason,
            action_summary: request.action_summary,
            diagnostic: request.diagnostic,
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            approval_id,
            PendingApproval {
                request: routed.clone(),
                response_tx: Some(tx),
            },
        );
        (routed, rx)
    }

    /// Wait for a decision and remove the pending request afterward.
    pub async fn wait_for_decision(
        &self,
        approval_id: &str,
        rx: oneshot::Receiver<ApprovalDecision>,
        timeout_duration: Duration,
    ) -> ApprovalDecision {
        let decision = match timeout(timeout_duration, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => ApprovalDecision::Denied,
            Err(_) => ApprovalDecision::TimedOut,
        };
        self.pending.lock().await.remove(approval_id);
        decision
    }

    /// Resolve a pending approval by id.
    pub async fn resolve(
        &self,
        approval_id: &str,
        user_id: &str,
        decision: ApprovalDecision,
    ) -> bool {
        let mut pending = self.pending.lock().await;
        let Some(entry) = pending.get_mut(approval_id) else {
            return false;
        };
        if entry.request.requesting_user_id != user_id {
            return false;
        }
        let Some(mut resolved) = pending.remove(approval_id) else {
            return false;
        };
        if let Some(tx) = resolved.response_tx.take() {
            let _ = tx.send(decision);
        }
        true
    }

    /// Snapshot all currently pending approval requests.
    pub async fn pending_requests(&self) -> Vec<PendingApprovalRequest> {
        self.pending
            .lock()
            .await
            .values()
            .map(|pending| pending.request.clone())
            .collect()
    }

    /// Fetch a single pending approval request by id without consuming it.
    pub async fn pending_request(&self, approval_id: &str) -> Option<PendingApprovalRequest> {
        self.pending
            .lock()
            .await
            .get(approval_id)
            .map(|pending| pending.request.clone())
    }
}

/// Gateway-backed requester that routes approval prompts to the source channel.
pub struct GatewayApprovalRequester {
    registry: Arc<ApprovalRegistry>,
    gateway: Arc<Gateway>,
    inbound: InboundMessage,
    timeout: Duration,
}

impl GatewayApprovalRequester {
    /// Create a requester bound to a specific inbound session route.
    pub fn new(
        registry: Arc<ApprovalRegistry>,
        gateway: Arc<Gateway>,
        inbound: InboundMessage,
        timeout: Duration,
    ) -> Self {
        Self {
            registry,
            gateway,
            inbound,
            timeout,
        }
    }
}

#[async_trait]
impl ApprovalRequester for GatewayApprovalRequester {
    async fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, FrameworkError> {
        let (pending, rx) = self.registry.register(request).await;
        let approval_message_id = self
            .gateway
            .send_approval_request(&self.inbound, &pending)
            .await?;
        let decision = self
            .registry
            .wait_for_decision(&pending.approval_id, rx, self.timeout)
            .await;
        if decision == ApprovalDecision::TimedOut
            && let Some(message_id) = approval_message_id
            && let Err(err) = self
                .gateway
                .delete_message(&self.inbound, &message_id)
                .await
        {
            warn!(
                status = "failed",
                error_kind = "approval_message_delete",
                approval_id = %pending.approval_id,
                channel_id = %self.inbound.channel_id,
                message_id,
                error = %err,
                "failed to delete expired approval message"
            );
        }
        Ok(decision)
    }
}

/// Fallback requester used when no user-facing approval route exists.
pub struct UnavailableApprovalRequester;

#[async_trait]
impl ApprovalRequester for UnavailableApprovalRequester {
    async fn request_approval(
        &self,
        _request: ApprovalRequest,
    ) -> Result<ApprovalDecision, FrameworkError> {
        Err(FrameworkError::Tool(
            "sandbox escalation required but no approval requester is available".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::{
        ApprovalDecision, ApprovalRegistry, ApprovalRequest, ApprovalRequester,
        GatewayApprovalRequester,
    };
    use crate::channels::{Channel, ChannelInbound, InboundMessage};
    use crate::config::{ChannelOutputMode, GatewayChannelKind, RoutingConfig};
    use crate::error::SandboxCapability;
    use crate::gateway::Gateway;

    fn test_request() -> ApprovalRequest {
        ApprovalRequest {
            agent_id: "agent-1".to_owned(),
            agent_name: "Agent One".to_owned(),
            session_id: "sess-1".to_owned(),
            requesting_user_id: "user-1".to_owned(),
            tool_name: "read".to_owned(),
            execution_kind: "preflight_escalation".to_owned(),
            capability: SandboxCapability::Read,
            reason: "outside sandbox".to_owned(),
            action_summary: "/tmp/secret.txt".to_owned(),
            diagnostic: "blocked".to_owned(),
        }
    }

    fn test_inbound() -> InboundMessage {
        InboundMessage {
            trace_id: "trace-1".to_owned(),
            source_channel: GatewayChannelKind::Discord,
            target_agent_id: "default".to_owned(),
            session_key: "agent:default:discord:chan-1".to_owned(),
            source_message_id: Some("msg-1".to_owned()),
            channel_id: "chan-1".to_owned(),
            guild_id: None,
            is_dm: true,
            user_id: "user-1".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            invoke: true,
            content: "run it".to_owned(),
        }
    }

    #[derive(Default)]
    struct ApprovalChannel {
        sent_approvals: Mutex<Vec<(String, String)>>,
        deleted_messages: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl Channel for ApprovalChannel {
        async fn send_message(
            &self,
            _channel_id: &str,
            _content: &str,
        ) -> Result<(), crate::error::FrameworkError> {
            Ok(())
        }

        async fn send_approval_request(
            &self,
            channel_id: &str,
            request: &crate::approval::PendingApprovalRequest,
        ) -> Result<Option<String>, crate::error::FrameworkError> {
            self.sent_approvals
                .lock()
                .await
                .push((channel_id.to_owned(), request.approval_id.clone()));
            Ok(Some("approval-message-1".to_owned()))
        }

        async fn delete_message(
            &self,
            channel_id: &str,
            message_id: &str,
        ) -> Result<(), crate::error::FrameworkError> {
            self.deleted_messages
                .lock()
                .await
                .push((channel_id.to_owned(), message_id.to_owned()));
            Ok(())
        }

        async fn add_reaction(
            &self,
            _channel_id: &str,
            _message_id: &str,
            _emoji: &str,
        ) -> Result<(), crate::error::FrameworkError> {
            Ok(())
        }

        async fn broadcast_typing(
            &self,
            _channel_id: &str,
        ) -> Result<(), crate::error::FrameworkError> {
            Ok(())
        }

        async fn listen(&self) -> Result<ChannelInbound, crate::error::FrameworkError> {
            pending::<Result<ChannelInbound, crate::error::FrameworkError>>().await
        }
    }

    #[tokio::test]
    async fn resolve_accepts_requesting_user() {
        let registry = ApprovalRegistry::new();
        let (pending, rx) = registry.register(test_request()).await;

        let resolved = registry
            .resolve(
                &pending.approval_id,
                &pending.requesting_user_id,
                ApprovalDecision::Approved,
            )
            .await;

        assert!(resolved);
        let decision = registry
            .wait_for_decision(&pending.approval_id, rx, std::time::Duration::from_secs(1))
            .await;
        assert_eq!(decision, ApprovalDecision::Approved);
    }

    #[tokio::test]
    async fn resolve_rejects_non_requesting_user_without_consuming_request() {
        let registry = ApprovalRegistry::new();
        let (pending, _rx) = registry.register(test_request()).await;

        let resolved = registry
            .resolve(
                &pending.approval_id,
                "other-user",
                ApprovalDecision::Approved,
            )
            .await;

        assert!(!resolved);
        let pending_requests = registry.pending_requests().await;
        assert_eq!(pending_requests.len(), 1);
        assert_eq!(pending_requests[0].approval_id, pending.approval_id);
    }

    #[tokio::test]
    async fn gateway_requester_deletes_expired_approval_message() {
        let registry = Arc::new(ApprovalRegistry::new());
        let channel = Arc::new(ApprovalChannel::default());
        let mut channels: HashMap<GatewayChannelKind, Arc<dyn Channel>> = HashMap::new();
        channels.insert(GatewayChannelKind::Discord, channel.clone());
        let gateway = Arc::new(Gateway::new(
            channels,
            HashMap::from([(GatewayChannelKind::Discord, ChannelOutputMode::Streaming)]),
            RoutingConfig::default(),
        ));
        let requester = GatewayApprovalRequester::new(
            Arc::clone(&registry),
            gateway,
            test_inbound(),
            std::time::Duration::from_millis(10),
        );

        let decision = requester
            .request_approval(test_request())
            .await
            .expect("approval request should return a timeout");

        assert_eq!(decision, ApprovalDecision::TimedOut);
        assert_eq!(
            channel.deleted_messages.lock().await.as_slice(),
            &[("chan-1".to_owned(), "approval-message-1".to_owned())]
        );
    }
}
