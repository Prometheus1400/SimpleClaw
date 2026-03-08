use crate::config::{GatewayChannelKind, InboundConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboundDecision {
    pub ingest_for_context: bool,
    pub allow_invoke: bool,
    pub target_agent_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct InboundPolicyContext {
    pub source_channel: GatewayChannelKind,
    pub workspace_id: Option<String>,
    pub channel_id: String,
    pub is_dm: bool,
    pub user_id: String,
    pub mentioned_bot: bool,
}

pub(crate) fn classify_inbound(
    inbound_policy: &InboundConfig,
    context: &InboundPolicyContext,
) -> InboundDecision {
    let policy = inbound_policy.resolve(
        context.source_channel,
        context.workspace_id.as_deref(),
        &context.channel_id,
        context.is_dm,
    );
    let target_agent_id = policy.agent.trim().to_owned();
    let allow_invoke = !target_agent_id.is_empty()
        && policy.allows_user(&context.user_id)
        && (!policy.require_mentions || context.mentioned_bot);
    let ingest_for_context = if context.is_dm { allow_invoke } else { true };
    InboundDecision {
        ingest_for_context,
        allow_invoke,
        target_agent_id,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{InboundPolicyContext, classify_inbound};
    use crate::config::{
        ChannelInboundConfig, GatewayChannelKind, InboundConfig, InboundPolicyConfig,
        WorkspaceInboundConfig,
    };

    fn context(
        workspace_id: Option<&str>,
        channel_id: &str,
        is_dm: bool,
        user_id: &str,
        mentioned_bot: bool,
    ) -> InboundPolicyContext {
        InboundPolicyContext {
            source_channel: GatewayChannelKind::Discord,
            workspace_id: workspace_id.map(str::to_owned),
            channel_id: channel_id.to_owned(),
            is_dm,
            user_id: user_id.to_owned(),
            mentioned_bot,
        }
    }

    #[test]
    fn rejects_disallowed_user() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["7".to_owned()]),
                require_mentions: Some(false),
            },
            ..InboundConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "9", true),
        );
        assert!(decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn rejects_disallowed_dm_user() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                ..InboundPolicyConfig::default()
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelInboundConfig {
                    policy: InboundPolicyConfig::default(),
                    dm: InboundPolicyConfig {
                        agent: Some("default".to_owned()),
                        allow_from: Some(vec!["7".to_owned()]),
                        require_mentions: Some(false),
                    },
                    workspaces: HashMap::new(),
                },
            )]),
            ..InboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, &context(None, "20", true, "9", true));
        assert!(!decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn rejects_when_mentions_required_but_missing() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..InboundConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "7", false),
        );
        assert!(decision.ingest_for_context);
        assert!(!decision.allow_invoke);
    }

    #[test]
    fn accepts_when_mentions_required_and_present() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..InboundConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "7", true),
        );
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
    }

    #[test]
    fn dm_ignores_mentions_requirement() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelInboundConfig {
                    policy: InboundPolicyConfig::default(),
                    dm: InboundPolicyConfig {
                        agent: Some("default".to_owned()),
                        allow_from: Some(vec!["7".to_owned()]),
                        require_mentions: Some(true),
                    },
                    workspaces: HashMap::new(),
                },
            )]),
            ..InboundConfig::default()
        };

        let decision = classify_inbound(&inbound_policy, &context(None, "20", true, "7", false));
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
    }

    #[test]
    fn channel_override_takes_precedence() {
        let inbound_policy = InboundConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelInboundConfig {
                    policy: InboundPolicyConfig {
                        agent: Some("reviewer".to_owned()),
                        allow_from: Some(vec!["2".to_owned()]),
                        require_mentions: Some(true),
                    },
                    dm: InboundPolicyConfig::default(),
                    workspaces: HashMap::from([(
                        "10".to_owned(),
                        WorkspaceInboundConfig {
                            policy: InboundPolicyConfig::default(),
                            channels: HashMap::from([(
                                "20".to_owned(),
                                InboundPolicyConfig {
                                    agent: Some("researcher".to_owned()),
                                    allow_from: Some(vec!["3".to_owned()]),
                                    require_mentions: Some(false),
                                },
                            )]),
                        },
                    )]),
                },
            )]),
            ..InboundConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "3", false),
        );
        assert!(decision.ingest_for_context);
        assert!(decision.allow_invoke);
        assert_eq!(decision.target_agent_id, "researcher");
    }
}
