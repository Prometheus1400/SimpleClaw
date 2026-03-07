use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::FrameworkError;
use crate::paths::AppPaths;
use crate::secrets::{SecretResolver, parse_secret_reference};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub sandbox: SandboxMode,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: None,
            sandbox: SandboxMode::default(),
            tools: ToolConfig::default(),
            skills: SkillsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedConfig {
    pub global: GlobalConfig,
}

impl LoadedConfig {
    pub fn load(workspace_override: Option<&Path>) -> Result<Self, FrameworkError> {
        let paths = AppPaths::resolve()?;
        paths.ensure_base_dir()?;

        let mut global = if paths.config_path.exists() {
            let content = fs::read_to_string(&paths.config_path)?;
            serde_yaml::from_str::<GlobalConfig>(&content)?
        } else {
            GlobalConfig::default()
        };
        global.resolve_secrets(&paths)?;
        global.database.path = paths.db_path;
        global.database.long_term_path = paths.long_term_db_path;
        normalize_agents_workspace_paths(&mut global.agents);
        if let Some(workspace_override) = workspace_override {
            let workspace = normalize_workspace_path(workspace_override);
            let default_id = global.agents.default.clone();
            let default_agent = global
                .agents
                .list
                .iter_mut()
                .find(|agent| agent.id == default_id)
                .ok_or_else(|| {
                    FrameworkError::Config(format!(
                        "agents.default '{}' does not match any agents.list id",
                        default_id
                    ))
                })?;
            default_agent.workspace = workspace;
        }
        validate_agents_config(&global.agents)?;
        validate_gateway_config(&global.gateway)?;
        validate_discord_inbound_config(&global.discord.inbound)?;

        Ok(Self { global })
    }
}

impl GlobalConfig {
    fn resolve_secrets(&mut self, paths: &AppPaths) -> Result<(), FrameworkError> {
        let resolver = SecretResolver::new(paths)?;
        self.provider.resolve_secrets(&resolver)?;
        self.discord.resolve_secrets(&resolver)?;
        Ok(())
    }
}

fn normalize_agents_workspace_paths(agents: &mut AgentsConfig) {
    agents.list = agents
        .list
        .iter()
        .map(|agent| AgentEntryConfig {
            id: agent.id.clone(),
            name: agent.name.clone(),
            workspace: normalize_workspace_path(&agent.workspace),
        })
        .collect();
}

fn validate_agents_config(agents: &AgentsConfig) -> Result<(), FrameworkError> {
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

fn validate_gateway_config(gateway: &GatewayConfig) -> Result<(), FrameworkError> {
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

fn validate_discord_inbound_config(inbound: &DiscordInboundConfig) -> Result<(), FrameworkError> {
    if inbound
        .defaults
        .agent
        .as_deref()
        .map(str::trim)
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        return Err(FrameworkError::Config(
            "discord.inbound.defaults.agent is required and must be non-empty".to_owned(),
        ));
    }
    validate_optional_policy_agent("discord.inbound.defaults", &inbound.defaults)?;
    validate_optional_policy_agent("discord.inbound.dm", &inbound.dm)?;
    for (server_id, server) in &inbound.servers {
        validate_optional_policy_agent(
            &format!("discord.inbound.servers.{server_id}"),
            &server.policy,
        )?;
        for (channel_id, policy) in &server.channels {
            validate_optional_policy_agent(
                &format!("discord.inbound.servers.{server_id}.channels.{channel_id}"),
                policy,
            )?;
        }
    }
    Ok(())
}

fn validate_optional_policy_agent(
    path: &str,
    policy: &DiscordInboundPolicyConfig,
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

fn normalize_workspace_path(path: &Path) -> PathBuf {
    let expanded = expand_env_vars(&path.to_string_lossy());
    expand_home_dir(&expanded).unwrap_or_else(|| PathBuf::from(expanded))
}

fn expand_home_dir(value: &str) -> Option<PathBuf> {
    if !value.starts_with('~') {
        return None;
    }
    if value.len() > 1 {
        let separator = value.as_bytes()[1];
        if separator != b'/' && separator != b'\\' {
            return None;
        }
    }

    let home = home_dir()?;
    if value == "~" {
        return Some(home);
    }

    let mut full = home;
    let remainder = &value[2..];
    if !remainder.is_empty() {
        full.push(remainder);
    }
    Some(full)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_env_vars(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] != '$' {
            output.push(chars[i]);
            i += 1;
            continue;
        }

        if i + 1 >= chars.len() {
            output.push('$');
            i += 1;
            continue;
        }

        if chars[i + 1] == '{' {
            let mut end = i + 2;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            if end < chars.len() {
                let key: String = chars[i + 2..end].iter().collect();
                if is_valid_env_key(&key) {
                    if let Ok(value) = env::var(&key) {
                        output.push_str(&value);
                    } else {
                        output.push_str(&format!("${{{key}}}"));
                    }
                    i = end + 1;
                    continue;
                }
            }
            output.push('$');
            i += 1;
            continue;
        }

        let mut end = i + 1;
        while end < chars.len() && is_env_key_char(chars[end], end == i + 1) {
            end += 1;
        }
        if end == i + 1 {
            output.push('$');
            i += 1;
            continue;
        }

        let key: String = chars[i + 1..end].iter().collect();
        if let Ok(value) = env::var(&key) {
            output.push_str(&value);
        } else {
            output.push('$');
            output.push_str(&key);
        }
        i = end;
    }

    output
}

fn is_valid_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    if !is_env_key_char(chars.next().unwrap_or('_'), true) {
        return false;
    }
    chars.all(|ch| is_env_key_char(ch, false))
}

fn is_env_key_char(ch: char, first: bool) -> bool {
    if first {
        ch == '_' || ch.is_ascii_alphabetic()
    } else {
        ch == '_' || ch.is_ascii_alphanumeric()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: PathBuf,
    #[serde(default = "default_long_term_db_path")]
    pub long_term_path: PathBuf,
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    #[serde(default = "default_busy_timeout_ms")]
    pub busy_timeout_ms: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            long_term_path: default_long_term_db_path(),
            pool_size: default_pool_size(),
            busy_timeout_ms: default_busy_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_kind")]
    pub kind: ProviderKind,
    #[serde(default = "default_provider_model")]
    pub model: String,
    #[serde(default = "default_provider_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: default_provider_kind(),
            model: default_provider_model(),
            api_base: default_provider_api_base(),
            api_key: None,
        }
    }
}

impl ProviderConfig {
    fn resolve_secrets(&mut self, resolver: &SecretResolver) -> Result<(), FrameworkError> {
        let Some(raw) = self.api_key.as_deref() else {
            return Ok(());
        };
        let secret_name = parse_secret_reference("provider.api_key", raw)?;
        let value = resolver.resolve(&secret_name).map_err(|err| {
            FrameworkError::Config(format!("provider.api_key failed to resolve: {err}"))
        })?;
        self.api_key = Some(value);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default = "default_history_messages")]
    pub history_messages: u32,
    #[serde(default)]
    pub memory_preinject: MemoryPreinjectConfig,
    #[serde(default)]
    pub summon_mode: SummonMode,
    #[serde(default = "default_safe_error_reply")]
    pub safe_error_reply: String,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub owner_ids: Vec<String>,
    #[serde(default)]
    pub exec_container: ExecContainerConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            history_messages: default_history_messages(),
            memory_preinject: MemoryPreinjectConfig::default(),
            summon_mode: SummonMode::default(),
            safe_error_reply: default_safe_error_reply(),
            log_level: LogLevel::default(),
            owner_ids: Vec::new(),
            exec_container: ExecContainerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecContainerConfig {
    pub image: String,
    pub network_enabled: bool,
    pub memory_mb: u32,
    pub cpus_milli: u32,
    pub pids_limit: u32,
    pub build_timeout_secs: u64,
    pub exec_timeout_secs: u64,
}

impl Default for ExecContainerConfig {
    fn default() -> Self {
        Self {
            image: default_exec_container_image(),
            network_enabled: default_exec_container_network_enabled(),
            memory_mb: default_exec_container_memory_mb(),
            cpus_milli: default_exec_container_cpus_milli(),
            pids_limit: default_exec_container_pids_limit(),
            build_timeout_secs: default_exec_container_build_timeout_secs(),
            exec_timeout_secs: default_exec_container_exec_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryPreinjectConfig {
    pub enabled: bool,
    pub top_k: u32,
    pub min_score: f32,
    pub max_items_per_store: u32,
    pub long_term_weight: f32,
    pub max_chars: u32,
}

impl Default for MemoryPreinjectConfig {
    fn default() -> Self {
        Self {
            enabled: default_memory_preinject_enabled(),
            top_k: default_memory_preinject_top_k(),
            min_score: default_memory_preinject_min_score(),
            max_items_per_store: default_memory_preinject_max_items_per_store(),
            long_term_weight: default_memory_preinject_long_term_weight(),
            max_chars: default_memory_preinject_max_chars(),
        }
    }
}

impl MemoryPreinjectConfig {
    pub fn normalized(&self) -> Self {
        Self {
            enabled: self.enabled,
            top_k: self.top_k.clamp(1, 10),
            min_score: self.min_score.clamp(0.0, 1.0),
            max_items_per_store: self.max_items_per_store.clamp(1, 100),
            long_term_weight: self.long_term_weight.clamp(0.0, 1.0),
            max_chars: self.max_chars.clamp(200, 4000),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    pub channels: Vec<GatewayChannelKind>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            channels: default_gateway_channels(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default = "default_agent_id")]
    pub default: String,
    #[serde(default = "default_agents_list")]
    pub list: Vec<AgentEntryConfig>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default: default_agent_id(),
            list: default_agents_list(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentEntryConfig {
    pub id: String,
    pub name: String,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GatewayChannelKind {
    Discord,
    Logging,
}

impl GatewayChannelKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Logging => "logging",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummonMode {
    #[default]
    Synchronous,
    Queued,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Gemini,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Gemini
    }
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
        }
    }

    pub fn supports_native_tools(self) -> bool {
        match self {
            Self::Gemini => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub inbound: DiscordInboundConfig,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            token: None,
            inbound: DiscordInboundConfig::default(),
        }
    }
}

impl DiscordConfig {
    fn resolve_secrets(&mut self, resolver: &SecretResolver) -> Result<(), FrameworkError> {
        let Some(raw) = self.token.as_deref() else {
            return Ok(());
        };
        let secret_name = parse_secret_reference("discord.token", raw)?;
        let value = resolver.resolve(&secret_name).map_err(|err| {
            FrameworkError::Config(format!("discord.token failed to resolve: {err}"))
        })?;
        self.token = Some(value);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordInboundConfig {
    #[serde(default)]
    pub defaults: DiscordInboundPolicyConfig,
    #[serde(default)]
    pub servers: HashMap<String, DiscordServerInboundConfig>,
    #[serde(default)]
    pub dm: DiscordInboundPolicyConfig,
}

impl Default for DiscordInboundConfig {
    fn default() -> Self {
        Self {
            defaults: DiscordInboundPolicyConfig {
                agent: Some(default_agent_id()),
                ..DiscordInboundPolicyConfig::default()
            },
            servers: HashMap::new(),
            dm: DiscordInboundPolicyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordServerInboundConfig {
    #[serde(flatten)]
    pub policy: DiscordInboundPolicyConfig,
    #[serde(default)]
    pub channels: HashMap<String, DiscordInboundPolicyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordInboundPolicyConfig {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,
    #[serde(default)]
    pub require_mentions: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct DiscordInboundPolicy {
    pub agent: String,
    pub allow_from: Option<Vec<String>>,
    pub require_mentions: bool,
}

impl DiscordInboundConfig {
    pub fn resolve(
        &self,
        guild_id: Option<u64>,
        channel_id: u64,
        is_dm: bool,
    ) -> DiscordInboundPolicy {
        let mut effective = self.defaults.clone();

        if is_dm {
            effective.apply_override(&self.dm);
            return effective.finalize(true);
        }

        if let Some(server) = guild_id.and_then(|id| self.servers.get(&id.to_string())) {
            effective.apply_override(&server.policy);
            if let Some(channel) = server.channels.get(&channel_id.to_string()) {
                effective.apply_override(channel);
            }
        }

        effective.finalize(false)
    }
}

impl DiscordInboundPolicyConfig {
    fn apply_override(&mut self, lower: &DiscordInboundPolicyConfig) {
        if let Some(agent) = lower.agent.as_deref() {
            self.agent = Some(agent.to_owned());
        }
        if let Some(allow_from) = &lower.allow_from {
            self.allow_from = Some(allow_from.clone());
        }
        if let Some(require_mentions) = lower.require_mentions {
            self.require_mentions = Some(require_mentions);
        }
    }

    fn finalize(self, is_dm: bool) -> DiscordInboundPolicy {
        DiscordInboundPolicy {
            agent: self.agent.unwrap_or_default(),
            allow_from: self.allow_from,
            require_mentions: if is_dm {
                false
            } else {
                self.require_mentions.unwrap_or(false)
            },
        }
    }
}

impl DiscordInboundPolicy {
    pub fn allows_user(&self, user_id: u64) -> bool {
        match &self.allow_from {
            Some(ids) => ids.iter().any(|id| id == &user_id.to_string()),
            None => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    #[serde(default = "default_embedding_model")]
    pub model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolConfig {
    pub enabled_tools: Vec<String>,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            enabled_tools: default_enabled_tools(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub enabled_skills: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled_skills: Vec::new(),
        }
    }
}

fn default_db_path() -> PathBuf {
    AppPaths::resolve()
        .map(|paths| paths.db_path)
        .unwrap_or_else(|_| PathBuf::from("~/.simpleclaw/db/lraf.db"))
}
fn default_long_term_db_path() -> PathBuf {
    AppPaths::resolve()
        .map(|paths| paths.long_term_db_path)
        .unwrap_or_else(|_| PathBuf::from("~/.simpleclaw/db/lraf_long_term.db"))
}
fn default_pool_size() -> usize {
    16
}
fn default_busy_timeout_ms() -> u64 {
    5_000
}
fn default_provider_kind() -> ProviderKind {
    ProviderKind::Gemini
}
fn default_provider_model() -> String {
    "gemini-2.0-flash".to_owned()
}
fn default_provider_api_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".to_owned()
}
fn default_max_steps() -> u32 {
    8
}
fn default_history_messages() -> u32 {
    10
}
fn default_memory_preinject_enabled() -> bool {
    true
}
fn default_memory_preinject_top_k() -> u32 {
    3
}
fn default_memory_preinject_min_score() -> f32 {
    0.72
}
fn default_memory_preinject_max_items_per_store() -> u32 {
    20
}
fn default_memory_preinject_long_term_weight() -> f32 {
    0.65
}
fn default_memory_preinject_max_chars() -> u32 {
    1200
}
fn default_safe_error_reply() -> String {
    "I hit an internal error while processing that request.".to_owned()
}
fn default_exec_container_image() -> String {
    "simpleclaw-sandbox:latest".to_owned()
}
fn default_exec_container_network_enabled() -> bool {
    true
}
fn default_exec_container_memory_mb() -> u32 {
    512
}
fn default_exec_container_cpus_milli() -> u32 {
    1000
}
fn default_exec_container_pids_limit() -> u32 {
    256
}
fn default_exec_container_build_timeout_secs() -> u64 {
    120
}
fn default_exec_container_exec_timeout_secs() -> u64 {
    20
}
fn default_agent_id() -> String {
    "default".to_owned()
}
fn default_agents_list() -> Vec<AgentEntryConfig> {
    vec![AgentEntryConfig {
        id: default_agent_id(),
        name: "Default".to_owned(),
        workspace: PathBuf::from("./workspace"),
    }]
}
fn default_gateway_channels() -> Vec<GatewayChannelKind> {
    vec![GatewayChannelKind::Discord]
}
fn default_enabled_tools() -> Vec<String> {
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
fn default_embedding_model() -> String {
    "all-MiniLM-L6-v2".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_config_{prefix}_{nanos}"))
    }

    fn test_paths(base_dir: PathBuf) -> AppPaths {
        let db_dir = base_dir.join("db");
        let logs_dir = base_dir.join("logs");
        let run_dir = base_dir.join("run");
        AppPaths {
            config_path: base_dir.join("config.yaml"),
            secrets_path: base_dir.join("secrets.yaml"),
            db_path: db_dir.join("lraf.db"),
            long_term_db_path: db_dir.join("lraf_long_term.db"),
            fastembed_cache_dir: base_dir.join(".fastembed_cache"),
            log_path: logs_dir.join("service.log"),
            pid_path: run_dir.join("service.pid"),
            base_dir,
            logs_dir,
            run_dir,
        }
    }

    #[test]
    fn channel_policy_overrides_server_and_global() {
        let mut inbound = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        inbound.servers.insert(
            "100".to_owned(),
            DiscordServerInboundConfig {
                policy: DiscordInboundPolicyConfig {
                    agent: Some("server".to_owned()),
                    allow_from: Some(vec!["2".to_owned()]),
                    require_mentions: None,
                },
                channels: HashMap::from([(
                    "200".to_owned(),
                    DiscordInboundPolicyConfig {
                        agent: Some("channel".to_owned()),
                        allow_from: Some(vec!["3".to_owned()]),
                        require_mentions: Some(false),
                    },
                )]),
            },
        );

        let policy = inbound.resolve(Some(100), 200, false);
        assert_eq!(policy.allow_from, Some(vec!["3".to_owned()]));
        assert!(!policy.require_mentions);
    }

    #[test]
    fn server_policy_applies_when_channel_missing() {
        let mut inbound = DiscordInboundConfig::default();
        inbound.defaults.agent = Some("default".to_owned());
        inbound.servers.insert(
            "100".to_owned(),
            DiscordServerInboundConfig {
                policy: DiscordInboundPolicyConfig {
                    agent: Some("server".to_owned()),
                    allow_from: Some(vec!["42".to_owned()]),
                    require_mentions: Some(true),
                },
                channels: HashMap::new(),
            },
        );

        let policy = inbound.resolve(Some(100), 201, false);
        assert_eq!(policy.allow_from, Some(vec!["42".to_owned()]));
        assert!(policy.require_mentions);
    }

    #[test]
    fn global_defaults_apply_when_no_server_or_channel_match() {
        let inbound = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["9".to_owned()]),
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        let policy = inbound.resolve(Some(999), 888, false);
        assert_eq!(policy.allow_from, Some(vec!["9".to_owned()]));
        assert!(policy.require_mentions);
    }

    #[test]
    fn dm_scope_overrides_defaults_and_forces_mentions_off() {
        let inbound = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            dm: DiscordInboundPolicyConfig {
                agent: Some("dm".to_owned()),
                allow_from: Some(vec!["11".to_owned()]),
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        let policy = inbound.resolve(None, 321, true);
        assert_eq!(policy.allow_from, Some(vec!["11".to_owned()]));
        assert!(!policy.require_mentions);
    }

    #[test]
    fn no_allow_from_defaults_to_allow_all() {
        let inbound = DiscordInboundConfig::default();
        let policy = inbound.resolve(Some(100), 200, false);
        assert!(policy.allows_user(123456789));
    }

    #[test]
    fn runtime_defaults_history_window() {
        let runtime = RuntimeConfig::default();
        assert_eq!(runtime.history_messages, 10);
        assert_eq!(runtime.log_level, LogLevel::Info);
        assert!(runtime.memory_preinject.enabled);
        assert_eq!(runtime.memory_preinject.top_k, 3);
        assert!((runtime.memory_preinject.min_score - 0.72).abs() < f32::EPSILON);
    }

    #[test]
    fn runtime_memory_preinject_accepts_overrides() {
        let yaml = r#"
memory_preinject:
  enabled: false
  top_k: 5
  min_score: 0.8
  max_items_per_store: 40
  long_term_weight: 0.7
  max_chars: 900
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml).expect("valid yaml");
        assert!(!parsed.memory_preinject.enabled);
        assert_eq!(parsed.memory_preinject.top_k, 5);
        assert!((parsed.memory_preinject.min_score - 0.8).abs() < f32::EPSILON);
        assert_eq!(parsed.memory_preinject.max_items_per_store, 40);
        assert!((parsed.memory_preinject.long_term_weight - 0.7).abs() < f32::EPSILON);
        assert_eq!(parsed.memory_preinject.max_chars, 900);
    }

    #[test]
    fn runtime_memory_preinject_rejects_unknown_fields() {
        let yaml = r#"
memory_preinject:
  enabled: true
  bogus: 1
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn runtime_memory_preinject_rejects_legacy_short_term_weight() {
        let yaml = r#"
memory_preinject:
  short_term_weight: 0.3
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn runtime_memory_preinject_normalized_clamps_values() {
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: 999,
            min_score: 5.0,
            max_items_per_store: 0,
            long_term_weight: -4.0,
            max_chars: 32,
        };
        let normalized = config.normalized();
        assert_eq!(normalized.top_k, 10);
        assert!((normalized.min_score - 1.0).abs() < f32::EPSILON);
        assert_eq!(normalized.max_items_per_store, 1);
        assert!((normalized.long_term_weight - 0.0).abs() < f32::EPSILON);
        assert_eq!(normalized.max_chars, 200);
    }

    #[test]
    fn runtime_log_level_accepts_debug() {
        let yaml = r#"
log_level: debug
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml).expect("valid yaml");
        assert_eq!(parsed.log_level, LogLevel::Debug);
    }

    #[test]
    fn runtime_log_level_rejects_unknown_value() {
        let yaml = r#"
log_level: verbose
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_unknown_provider_kind() {
        let yaml = r#"
kind: not_a_provider
model: gemini-2.0-flash
api_base: https://example.com
api_key: "${secret:gemini_api_key}"
"#;
        let parsed = serde_yaml::from_str::<ProviderConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_legacy_provider_api_key_env_field() {
        let yaml = r#"
kind: gemini
model: gemini-2.0-flash
api_base: https://example.com
api_key: "${secret:gemini_api_key}"
api_key_env: GEMINI_API_KEY
"#;
        let parsed = serde_yaml::from_str::<ProviderConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_legacy_discord_token_env_field() {
        let yaml = r#"
token: "${secret:discord_token}"
token_env: DISCORD_TOKEN
"#;
        let parsed = serde_yaml::from_str::<DiscordConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_unknown_gateway_channel_kind() {
        let yaml = r#"
channels:
  - not_a_channel
"#;
        let parsed = serde_yaml::from_str::<GatewayConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn tool_config_defaults_to_all_builtin_tools() {
        let parsed: ToolConfig = serde_yaml::from_str("{}\n").expect("valid yaml");
        assert_eq!(
            parsed.enabled_tools,
            vec![
                "memory".to_owned(),
                "memorize".to_owned(),
                "forget".to_owned(),
                "summon".to_owned(),
                "task".to_owned(),
                "web_search".to_owned(),
                "clock".to_owned(),
                "web_fetch".to_owned(),
                "read".to_owned(),
                "edit".to_owned(),
                "exec".to_owned(),
                "process".to_owned()
            ]
        );
    }

    #[test]
    fn tool_config_rejects_legacy_boolean_fields() {
        let parsed = serde_yaml::from_str::<ToolConfig>("memory: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_supports_enabled_tools_allowlist() {
        let parsed: AgentConfig = serde_yaml::from_str(
            r#"
tools:
  enabled_tools:
    - memory
    - summon
"#,
        )
        .expect("valid yaml");
        assert_eq!(
            parsed.tools.enabled_tools,
            vec!["memory".to_owned(), "summon".to_owned()]
        );
    }

    #[test]
    fn agent_config_supports_enabled_skills_allowlist() {
        let parsed: AgentConfig = serde_yaml::from_str(
            r#"
skills:
  enabled_skills:
    - code_review
    - release_checklist
"#,
        )
        .expect("valid yaml");
        assert_eq!(
            parsed.skills.enabled_skills,
            vec!["code_review".to_owned(), "release_checklist".to_owned()]
        );
    }

    #[test]
    fn agent_config_defaults_exec_policy() {
        let parsed: AgentConfig = serde_yaml::from_str("{}\n").expect("valid yaml");
        assert_eq!(parsed.sandbox, SandboxMode::On);
    }

    #[test]
    fn agent_config_defaults_skills_config() {
        let parsed: AgentConfig = serde_yaml::from_str("{}\n").expect("valid yaml");
        assert!(parsed.skills.enabled_skills.is_empty());
    }

    #[test]
    fn agent_config_skills_rejects_directory_field() {
        let parsed = serde_yaml::from_str::<AgentConfig>(
            r#"
skills:
  enabled_skills:
    - code_review
  directory: skills
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_network_allow_all_field() {
        let parsed = serde_yaml::from_str::<AgentConfig>("network_allow_all: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_read_allow_all_field() {
        let parsed = serde_yaml::from_str::<AgentConfig>("read_allow_all: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_sandbox_accepts_off() {
        let parsed: AgentConfig = serde_yaml::from_str(
            r#"
sandbox: off
"#,
        )
        .expect("valid yaml");
        assert_eq!(parsed.sandbox, SandboxMode::Off);
    }

    #[test]
    fn agent_config_sandbox_accepts_on() {
        let parsed: AgentConfig = serde_yaml::from_str(
            r#"
sandbox: on
"#,
        )
        .expect("valid yaml");
        assert_eq!(parsed.sandbox, SandboxMode::On);
    }

    #[test]
    fn agent_config_sandbox_rejects_wasm() {
        let parsed = serde_yaml::from_str::<AgentConfig>(
            r#"
sandbox: wasm
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_sandbox_rejects_workspace() {
        let parsed = serde_yaml::from_str::<AgentConfig>(
            r#"
sandbox: workspace
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_name_field() {
        let yaml = r#"
name: legacy
"#;
        let parsed = serde_yaml::from_str::<AgentConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_routing_field() {
        let yaml = r#"
routing:
  channel: discord
"#;
        let parsed = serde_yaml::from_str::<AgentConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn global_config_resolves_required_secret_references() {
        let provider_env = "SIMPLECLAW_TEST_PROVIDER_SECRET";
        let discord_env = "SIMPLECLAW_TEST_DISCORD_SECRET";
        unsafe {
            std::env::set_var(provider_env, "provider-secret");
            std::env::set_var(discord_env, "discord-secret");
        }

        let dir = unique_test_dir("resolve_refs");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig {
            provider: ProviderConfig {
                api_key: Some(format!("${{secret:{provider_env}}}")),
                ..ProviderConfig::default()
            },
            discord: DiscordConfig {
                token: Some(format!("${{secret:{discord_env}}}")),
                ..DiscordConfig::default()
            },
            ..GlobalConfig::default()
        };

        global
            .resolve_secrets(&paths)
            .expect("secret references should resolve");
        assert_eq!(global.provider.api_key.as_deref(), Some("provider-secret"));
        assert_eq!(global.discord.token.as_deref(), Some("discord-secret"));

        unsafe {
            std::env::remove_var(provider_env);
            std::env::remove_var(discord_env);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn global_config_rejects_plaintext_secret_values() {
        let dir = unique_test_dir("reject_plaintext");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig {
            provider: ProviderConfig {
                api_key: Some("plaintext-key".to_owned()),
                ..ProviderConfig::default()
            },
            discord: DiscordConfig {
                token: Some("${secret:discord_token}".to_owned()),
                ..DiscordConfig::default()
            },
            ..GlobalConfig::default()
        };

        let err = global.resolve_secrets(&paths).unwrap_err();
        assert!(
            err.to_string()
                .contains("provider.api_key must use secret reference syntax")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn global_config_allows_missing_optional_secret_fields() {
        let dir = unique_test_dir("missing_optional");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig::default();
        global
            .resolve_secrets(&paths)
            .expect("missing optional secret fields should not fail config load");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn normalize_workspace_path_expands_home_prefix() {
        let Some(home) = home_dir() else {
            return;
        };
        let normalized = normalize_workspace_path(Path::new("~/workspace"));
        assert_eq!(normalized, home.join("workspace"));
    }

    #[test]
    fn normalize_workspace_path_expands_dollar_env_syntax() {
        let key = "SIMPLECLAW_TEST_WORKSPACE_ROOT";
        unsafe {
            std::env::set_var(key, "/tmp/simpleclaw-workspace-root");
        }
        let normalized = normalize_workspace_path(Path::new("${SIMPLECLAW_TEST_WORKSPACE_ROOT}/a"));
        assert_eq!(
            normalized,
            PathBuf::from("/tmp/simpleclaw-workspace-root/a")
        );
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn normalize_agents_workspace_paths_updates_entries() {
        let key = "SIMPLECLAW_TEST_SUMMON_ROOT";
        unsafe {
            std::env::set_var(key, "/tmp/simpleclaw-summon-root");
        }
        let mut agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![
                AgentEntryConfig {
                    id: "default".to_owned(),
                    name: "Default".to_owned(),
                    workspace: PathBuf::from("$SIMPLECLAW_TEST_SUMMON_ROOT/default"),
                },
                AgentEntryConfig {
                    id: "researcher".to_owned(),
                    name: "Researcher".to_owned(),
                    workspace: PathBuf::from("$SIMPLECLAW_TEST_SUMMON_ROOT/research"),
                },
            ],
        };

        normalize_agents_workspace_paths(&mut agents);

        assert_eq!(
            agents
                .list
                .iter()
                .find(|agent| agent.id == "default")
                .expect("default target should exist")
                .workspace,
            PathBuf::from("/tmp/simpleclaw-summon-root/default")
        );
        assert_eq!(
            agents
                .list
                .iter()
                .find(|agent| agent.id == "researcher")
                .expect("summon target should exist"),
            &AgentEntryConfig {
                id: "researcher".to_owned(),
                name: "Researcher".to_owned(),
                workspace: PathBuf::from("/tmp/simpleclaw-summon-root/research")
            }
        );

        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn validate_agents_config_rejects_duplicate_ids() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![
                AgentEntryConfig {
                    id: "default".to_owned(),
                    name: "Default".to_owned(),
                    workspace: PathBuf::from("./workspace"),
                },
                AgentEntryConfig {
                    id: "default".to_owned(),
                    name: "Duplicate".to_owned(),
                    workspace: PathBuf::from("./other"),
                },
            ],
        };
        assert!(validate_agents_config(&agents).is_err());
    }

    #[test]
    fn validate_agents_config_rejects_missing_default_id() {
        let agents = AgentsConfig {
            default: "missing".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
            }],
        };
        assert!(validate_agents_config(&agents).is_err());
    }

    #[test]
    fn runtime_config_rejects_legacy_workspace_fields() {
        let yaml = r#"
default_agent_workspace: ./workspace
summon_agents:
  researcher: ./agents/researcher
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn runtime_config_rejects_exec_policy_fields_moved_to_agent_config() {
        let yaml = r#"
network_allow_all: false
read_allow_all: false
sandbox: off
"#;
        let parsed = serde_yaml::from_str::<RuntimeConfig>(yaml);
        assert!(parsed.is_err());
    }
}
