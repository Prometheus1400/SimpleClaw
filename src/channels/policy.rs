use crate::config::{GatewayChannelKind, RoutingConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InboundDecision {
    Drop,
    ContextOnly { agent_id: String },
    Invoke { agent_id: String },
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
    inbound_policy: &RoutingConfig,
    context: &InboundPolicyContext,
) -> InboundDecision {
    let policy = inbound_policy.resolve(
        context.source_channel,
        context.workspace_id.as_deref(),
        &context.channel_id,
        context.is_dm,
    );
    let agent_id = policy.agent.trim().to_owned();
    let allow_invoke =
        policy.allows_user(&context.user_id) && (!policy.require_mentions || context.mentioned_bot);
    if allow_invoke {
        InboundDecision::Invoke { agent_id }
    } else if context.is_dm {
        InboundDecision::Drop
    } else {
        InboundDecision::ContextOnly { agent_id }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{InboundDecision, InboundPolicyContext, classify_inbound};
    use crate::config::{
        ChannelRoutingConfig, GatewayChannelKind, InboundPolicyConfig, RoutingConfig,
        WorkspaceRoutingConfig,
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
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["7".to_owned()]),
                require_mentions: Some(false),
            },
            ..RoutingConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "9", true),
        );
        assert_eq!(
            decision,
            InboundDecision::ContextOnly {
                agent_id: "default".to_owned()
            }
        );
    }

    #[test]
    fn rejects_disallowed_dm_user() {
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                ..InboundPolicyConfig::default()
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelRoutingConfig {
                    defaults: InboundPolicyConfig::default(),
                    dm: InboundPolicyConfig {
                        agent: Some("default".to_owned()),
                        allow_from: Some(vec!["7".to_owned()]),
                        require_mentions: Some(false),
                    },
                    workspaces: HashMap::new(),
                },
            )]),
        };

        let decision = classify_inbound(&inbound_policy, &context(None, "20", true, "9", true));
        assert_eq!(decision, InboundDecision::Drop);
    }

    #[test]
    fn rejects_when_mentions_required_but_missing() {
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "7", false),
        );
        assert_eq!(
            decision,
            InboundDecision::ContextOnly {
                agent_id: "default".to_owned()
            }
        );
    }

    #[test]
    fn accepts_when_mentions_required_and_present() {
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "7", true),
        );
        assert_eq!(
            decision,
            InboundDecision::Invoke {
                agent_id: "default".to_owned()
            }
        );
    }

    #[test]
    fn dm_ignores_mentions_requirement() {
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelRoutingConfig {
                    defaults: InboundPolicyConfig::default(),
                    dm: InboundPolicyConfig {
                        agent: Some("default".to_owned()),
                        allow_from: Some(vec!["7".to_owned()]),
                        require_mentions: Some(true),
                    },
                    workspaces: HashMap::new(),
                },
            )]),
        };

        let decision = classify_inbound(&inbound_policy, &context(None, "20", true, "7", false));
        assert_eq!(
            decision,
            InboundDecision::Invoke {
                agent_id: "default".to_owned()
            }
        );
    }

    #[test]
    fn channel_override_takes_precedence() {
        let inbound_policy = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            channels: HashMap::from([(
                GatewayChannelKind::Discord,
                ChannelRoutingConfig {
                    defaults: InboundPolicyConfig {
                        agent: Some("reviewer".to_owned()),
                        allow_from: Some(vec!["2".to_owned()]),
                        require_mentions: Some(true),
                    },
                    dm: InboundPolicyConfig::default(),
                    workspaces: HashMap::from([(
                        "10".to_owned(),
                        WorkspaceRoutingConfig {
                            defaults: InboundPolicyConfig::default(),
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
        };

        let decision = classify_inbound(
            &inbound_policy,
            &context(Some("10"), "20", false, "3", false),
        );
        assert_eq!(
            decision,
            InboundDecision::Invoke {
                agent_id: "researcher".to_owned()
            }
        );
    }
}
