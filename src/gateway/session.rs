use crate::config::GatewayChannelKind;

pub(super) fn build_session_key(
    agent_id: &str,
    is_dm: bool,
    source: GatewayChannelKind,
    channel_id: &str,
) -> String {
    if is_dm {
        format!("agent:{agent_id}:main")
    } else {
        match source {
            GatewayChannelKind::Discord => format!("agent:{agent_id}:discord:{channel_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_session_key;
    use crate::config::GatewayChannelKind;

    #[test]
    fn dm_session_key_uses_main_suffix() {
        assert_eq!(
            build_session_key("default", true, GatewayChannelKind::Discord, "123"),
            "agent:default:main"
        );
    }

    #[test]
    fn non_dm_session_key_uses_discord_channel() {
        assert_eq!(
            build_session_key("default", false, GatewayChannelKind::Discord, "123"),
            "agent:default:discord:123"
        );
    }
}
