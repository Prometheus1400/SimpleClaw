use std::path::PathBuf;

use crate::paths::AppPaths;

use super::agents::AgentEntryConfig;
use super::execution::AgentConfig;
use super::gateway::GatewayChannelKind;

pub(super) fn default_db_path() -> PathBuf {
    AppPaths::resolve()
        .map(|paths| paths.db_path)
        .unwrap_or_else(|_| PathBuf::from("~/.simpleclaw/db/lraf.db"))
}

pub(super) fn default_long_term_db_path() -> PathBuf {
    AppPaths::resolve()
        .map(|paths| paths.long_term_db_path)
        .unwrap_or_else(|_| PathBuf::from("~/.simpleclaw/db/lraf_long_term.db"))
}

pub(super) fn default_pool_size() -> usize {
    16
}

pub(super) fn default_busy_timeout_ms() -> u64 {
    5_000
}

pub(super) fn default_provider_key() -> String {
    "default".to_owned()
}

pub(super) fn default_provider_model() -> String {
    "gemini-2.0-flash".to_owned()
}

pub(super) fn default_provider_api_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".to_owned()
}

pub(super) fn default_max_steps() -> u32 {
    8
}

pub(super) fn default_history_messages() -> u32 {
    10
}

pub(super) fn default_memory_preinject_enabled() -> bool {
    true
}

pub(super) fn default_memory_preinject_top_k() -> u32 {
    3
}

pub(super) fn default_memory_preinject_min_score() -> f32 {
    0.72
}

pub(super) fn default_memory_preinject_long_term_weight() -> f32 {
    0.65
}

pub(super) fn default_memory_preinject_max_chars() -> u32 {
    1200
}

pub(super) fn default_safe_error_reply() -> String {
    "I hit an internal error while processing that request.".to_owned()
}

pub(super) fn default_sandbox_enabled() -> bool {
    true
}

pub(super) fn default_agent_id() -> String {
    "default".to_owned()
}

pub(super) fn default_agents_list() -> Vec<AgentEntryConfig> {
    vec![AgentEntryConfig {
        id: default_agent_id(),
        name: "Default".to_owned(),
        workspace: PathBuf::from("./workspace"),
        runtime: AgentConfig::default(),
    }]
}

pub(super) fn default_gateway_channels() -> Vec<GatewayChannelKind> {
    vec![GatewayChannelKind::Discord]
}

pub(super) fn default_enabled_tools() -> Vec<String> {
    [
        "memory",
        "memorize",
        "forget",
        "summon",
        "task",
        "web_search",
        "clock",
        "web_fetch",
        "read",
        "edit",
        "exec",
        "process",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect()
}

pub(super) fn default_embedding_model() -> String {
    "all-MiniLM-L6-v2".to_owned()
}
