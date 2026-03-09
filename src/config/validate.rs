use std::collections::HashSet;

use crate::error::FrameworkError;

use super::agents::AgentsConfig;
use super::defaults::default_agent_id;
use super::gateway::GatewayConfig;
use super::providers::ProvidersConfig;
use super::routing::{InboundConfig, InboundPolicyConfig};

pub(super) fn validate_agents_config(agents: &AgentsConfig) -> Result<(), FrameworkError> {
    if agents.list.is_empty() {
        return Err(FrameworkError::Config(
            "agents.list must include at least one agent".to_owned(),
        ));
    }

    let mut ids = HashSet::new();
    for agent in &agents.list {
        if agent.id.trim().is_empty() {
            return Err(FrameworkError::Config(
                "agents.list entries must have a non-empty id".to_owned(),
            ));
        }
        if agent.name.trim().is_empty() {
            return Err(FrameworkError::Config(
                "agents.list entries must have a non-empty name".to_owned(),
            ));
        }
        if !ids.insert(agent.id.clone()) {
            return Err(FrameworkError::Config(format!(
                "agents.list contains duplicate id: {}",
                agent.id
            )));
        }
    }

    if !ids.contains(&agents.default) {
        return Err(FrameworkError::Config(format!(
            "agents.default '{}' does not match any agents.list id",
            agents.default
        )));
    }

    Ok(())
}

pub(super) fn validate_providers_config(
    providers: &ProvidersConfig,
) -> Result<(), FrameworkError> {
    if providers.entries.is_empty() {
        return Err(FrameworkError::Config(
            "providers.entries must include at least one provider".to_owned(),
        ));
    }
    if !providers.entries.contains_key(&providers.default) {
        return Err(FrameworkError::Config(format!(
            "providers.default '{}' does not match any providers.entries key",
            providers.default
        )));
    }
    Ok(())
}

pub(super) fn reconcile_inbound_default_agent(
    inbound: &mut InboundConfig,
    agents: &AgentsConfig,
) {
    let Some(current_agent_raw) = inbound.defaults.agent.as_deref() else {
        inbound.defaults.agent = Some(agents.default.clone());
        return;
    };
    let current_agent = current_agent_raw.trim();
    if current_agent.is_empty() {
        inbound.defaults.agent = Some(agents.default.clone());
        return;
    }

    let legacy_default = default_agent_id();
    if current_agent != legacy_default {
        return;
    }
    let legacy_exists = agents.list.iter().any(|agent| agent.id == legacy_default);
    if legacy_exists {
        return;
    }
    if agents.list.iter().any(|agent| agent.id == agents.default) {
        inbound.defaults.agent = Some(agents.default.clone());
    }
}

pub(super) fn validate_gateway_config(gateway: &GatewayConfig) -> Result<(), FrameworkError> {
    if gateway.channels.is_empty() {
        return Err(FrameworkError::Config(
            "gateway.channels must include at least one channel".to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    for channel in &gateway.channels {
        if !seen.insert(*channel) {
            return Err(FrameworkError::Config(format!(
                "gateway.channels contains duplicate entry: {}",
                channel.as_str()
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_inbound_config(inbound: &InboundConfig) -> Result<(), FrameworkError> {
    if inbound
        .defaults
        .agent
        .as_deref()
        .map(str::trim)
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        return Err(FrameworkError::Config(
            "inbound.defaults.agent is required and must be non-empty".to_owned(),
        ));
    }
    validate_optional_policy_agent("inbound.defaults", &inbound.defaults)?;
    for (kind, channel) in &inbound.channels {
        validate_optional_policy_agent(
            &format!("inbound.channels.{}", kind.as_str()),
            &channel.policy,
        )?;
        validate_optional_policy_agent(
            &format!("inbound.channels.{}.dm", kind.as_str()),
            &channel.dm,
        )?;
        for (workspace_id, workspace) in &channel.workspaces {
            validate_optional_policy_agent(
                &format!(
                    "inbound.channels.{}.workspaces.{workspace_id}",
                    kind.as_str()
                ),
                &workspace.policy,
            )?;
            for (channel_id, policy) in &workspace.channels {
                validate_optional_policy_agent(
                    &format!(
                        "inbound.channels.{}.workspaces.{workspace_id}.channels.{channel_id}",
                        kind.as_str()
                    ),
                    policy,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_optional_policy_agent(
    path: &str,
    policy: &InboundPolicyConfig,
) -> Result<(), FrameworkError> {
    if let Some(agent) = policy.agent.as_deref()
        && agent.trim().is_empty()
    {
        return Err(FrameworkError::Config(format!(
            "{path}.agent must be non-empty when provided"
        )));
    }
    Ok(())
}
