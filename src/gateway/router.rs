use crate::channels::{ChannelInbound, InboundMessage};
use crate::config::{GatewayChannelKind, InboundConfig};

use super::policy::evaluate_inbound_policy;
use super::session::build_session_key;

pub(super) fn route_inbound(
    kind: GatewayChannelKind,
    inbound: ChannelInbound,
    inbound_policy: &InboundConfig,
) -> Option<InboundMessage> {
    let decision = evaluate_inbound_policy(kind, &inbound, inbound_policy);
    if !decision.ingest_for_context {
        return None;
    }

    let target_agent_id = decision.target_agent_id.clone();

    Some(InboundMessage {
        trace_id: crate::telemetry::next_trace_id(),
        source_channel: kind,
        target_agent_id: target_agent_id.clone(),
        session_key: build_session_key(&target_agent_id, inbound.is_dm, kind, &inbound.channel_id),
        channel_id: inbound.channel_id,
        guild_id: inbound.guild_id,
        is_dm: inbound.is_dm,
        user_id: inbound.user_id,
        username: inbound.username,
        mentioned_bot: inbound.mentioned_bot,
        invoke: decision.allow_invoke,
        content: inbound.content,
    })
}

#[cfg(test)]
mod tests {
    use super::route_inbound;
    use crate::channels::ChannelInbound;
    use crate::config::{GatewayChannelKind, InboundConfig, InboundPolicyConfig};

    #[test]
    fn route_inbound_sets_policy_and_session_fields() {
        let inbound = ChannelInbound {
            channel_id: "123".to_owned(),
            guild_id: Some("10".to_owned()),
            is_dm: false,
            user_id: "7".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            content: "hello".to_owned(),
        };
        let message = route_inbound(
            GatewayChannelKind::Discord,
            inbound,
            &InboundConfig::default(),
        )
        .expect("message should be routed");
        assert!(message.invoke);
        assert_eq!(message.target_agent_id, "default");
        assert_eq!(message.session_key, "agent:default:discord:123");
    }

    #[test]
    fn route_inbound_drops_dm_when_policy_denies_ingest() {
        let inbound = ChannelInbound {
            channel_id: "123".to_owned(),
            guild_id: None,
            is_dm: true,
            user_id: "7".to_owned(),
            username: "kaleb".to_owned(),
            mentioned_bot: false,
            content: "hello".to_owned(),
        };
        let policy = InboundConfig {
            dm: InboundPolicyConfig {
                allow_from: Some(vec!["999".to_owned()]),
                ..InboundPolicyConfig::default()
            },
            ..InboundConfig::default()
        };
        let message = route_inbound(GatewayChannelKind::Discord, inbound, &policy);
        assert!(message.is_none());
    }
}
