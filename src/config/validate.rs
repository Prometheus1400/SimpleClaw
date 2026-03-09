use std::collections::HashSet;

use crate::error::FrameworkError;

use super::agents::AgentsConfig;
use super::defaults::default_agent_id;
use super::gateway::GatewayConfig;
use super::providers::ProvidersConfig;
use super::routing::{InboundPolicyConfig, RoutingConfig};
use super::tools::ToolsConfig;
use super::tools::WebSearchProvider;

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
        validate_tools_config(&agent.id, &agent.config.tools)?;
    }

    if !ids.contains(&agents.default) {
        return Err(FrameworkError::Config(format!(
            "agents.default '{}' does not match any agents.list id",
            agents.default
        )));
    }

    Ok(())
}

fn validate_tools_config(agent_id: &str, tools: &ToolsConfig) -> Result<(), FrameworkError> {
    if let Some(memory) = &tools.memory {
        validate_optional_nonzero_u32(
            agent_id,
            "tools.memory.default_top_k",
            memory.default_top_k,
        )?;
        validate_optional_nonzero_u32(agent_id, "tools.memory.max_top_k", memory.max_top_k)?;
    }
    if let Some(web_search) = &tools.web_search {
        validate_optional_nonzero_u64(
            agent_id,
            "tools.web_search.timeout_seconds",
            web_search.timeout_seconds,
        )?;
        match web_search.provider {
            WebSearchProvider::Brave => {
                if web_search.enabled
                    && web_search
                        .api_key
                        .as_deref()
                        .map(str::trim)
                        .map(str::is_empty)
                        .unwrap_or(true)
                {
                    return Err(FrameworkError::Config(format!(
                        "agents.list[{agent_id}].config.tools.web_search.api_key is required when tools.web_search.provider=brave and enabled=true"
                    )));
                }
            }
            WebSearchProvider::Duckduckgo => {
                if web_search.api_key.is_some() {
                    return Err(FrameworkError::Config(format!(
                        "agents.list[{agent_id}].config.tools.web_search.api_key is not allowed when tools.web_search.provider=duckduckgo"
                    )));
                }
            }
        }
    }
    if let Some(web_fetch) = &tools.web_fetch {
        validate_optional_nonzero_u64(
            agent_id,
            "tools.web_fetch.timeout_seconds",
            web_fetch.timeout_seconds,
        )?;
        validate_optional_nonzero_u32(agent_id, "tools.web_fetch.max_chars", web_fetch.max_chars)?;
    }
    if let Some(read) = &tools.read {
        validate_optional_nonzero_u64(
            agent_id,
            "tools.read.timeout_seconds",
            read.timeout_seconds,
        )?;
    }
    if let Some(edit) = &tools.edit {
        validate_optional_nonzero_u64(
            agent_id,
            "tools.edit.timeout_seconds",
            edit.timeout_seconds,
        )?;
    }
    if let Some(exec) = &tools.exec {
        validate_optional_nonzero_u64(
            agent_id,
            "tools.exec.timeout_seconds",
            exec.timeout_seconds,
        )?;
    }
    Ok(())
}

fn validate_optional_nonzero_u32(
    agent_id: &str,
    field: &str,
    value: Option<u32>,
) -> Result<(), FrameworkError> {
    if let Some(value) = value
        && value == 0
    {
        return Err(FrameworkError::Config(format!(
            "agents.list[{agent_id}].config.{field} must be > 0 when provided"
        )));
    }
    Ok(())
}

fn validate_optional_nonzero_u64(
    agent_id: &str,
    field: &str,
    value: Option<u64>,
) -> Result<(), FrameworkError> {
    if let Some(value) = value
        && value == 0
    {
        return Err(FrameworkError::Config(format!(
            "agents.list[{agent_id}].config.{field} must be > 0 when provided"
        )));
    }
    Ok(())
}

pub(super) fn validate_providers_config(providers: &ProvidersConfig) -> Result<(), FrameworkError> {
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

pub(super) fn reconcile_routing_default_agent(routing: &mut RoutingConfig, agents: &AgentsConfig) {
    let Some(current_agent_raw) = routing.defaults.agent.as_deref() else {
        routing.defaults.agent = Some(agents.default.clone());
        return;
    };
    let current_agent = current_agent_raw.trim();
    if current_agent.is_empty() {
        routing.defaults.agent = Some(agents.default.clone());
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
        routing.defaults.agent = Some(agents.default.clone());
    }
}

pub(super) fn validate_gateway_config(gateway: &GatewayConfig) -> Result<(), FrameworkError> {
    let has_enabled = gateway.channels.values().any(|ch| ch.enabled);
    if gateway.channels.is_empty() || !has_enabled {
        return Err(FrameworkError::Config(
            "gateway.channels must include at least one enabled channel".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_routing_config(routing: &RoutingConfig) -> Result<(), FrameworkError> {
    if routing
        .defaults
        .agent
        .as_deref()
        .map(str::trim)
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        return Err(FrameworkError::Config(
            "gateway.routing.defaults.agent is required and must be non-empty".to_owned(),
        ));
    }
    validate_optional_policy_agent("gateway.routing.defaults", &routing.defaults)?;
    for (kind, channel) in &routing.channels {
        validate_optional_policy_agent(
            &format!("gateway.routing.channels.{}.defaults", kind.as_str()),
            &channel.defaults,
        )?;
        validate_optional_policy_agent(
            &format!("gateway.routing.channels.{}.dm", kind.as_str()),
            &channel.dm,
        )?;
        for (workspace_id, workspace) in &channel.workspaces {
            validate_optional_policy_agent(
                &format!(
                    "gateway.routing.channels.{}.workspaces.{workspace_id}.defaults",
                    kind.as_str()
                ),
                &workspace.defaults,
            )?;
            for (channel_id, policy) in &workspace.channels {
                validate_optional_policy_agent(
                    &format!(
                        "gateway.routing.channels.{}.workspaces.{workspace_id}.channels.{channel_id}",
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
