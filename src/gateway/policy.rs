use crate::channels::ChannelInbound;
use crate::channels::policy::{InboundDecision, InboundPolicyContext, classify_inbound};
use crate::config::{GatewayChannelKind, InboundConfig};

pub(super) fn evaluate_inbound_policy(
    kind: GatewayChannelKind,
    inbound: &ChannelInbound,
    inbound_policy: &InboundConfig,
) -> InboundDecision {
    let context = InboundPolicyContext {
        source_channel: kind,
        workspace_id: inbound.guild_id.clone(),
        channel_id: inbound.channel_id.clone(),
        is_dm: inbound.is_dm,
        user_id: inbound.user_id.clone(),
        mentioned_bot: inbound.mentioned_bot,
    };
    classify_inbound(inbound_policy, &context)
}
