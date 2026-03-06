use std::collections::HashMap;
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
    pub discord: DiscordConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_agent_name")]
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: default_agent_name(),
            model: None,
            tools: ToolConfig::default(),
            routing: RoutingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedConfig {
    pub global: GlobalConfig,
    pub agent: AgentConfig,
    pub workspace: PathBuf,
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

        let workspace = workspace_override
            .map(PathBuf::from)
            .unwrap_or_else(|| global.runtime.default_agent_workspace.clone());
        let agent_path = workspace.join("agent.yaml");
        let agent = if agent_path.exists() {
            let content = fs::read_to_string(agent_path)?;
            serde_yaml::from_str::<AgentConfig>(&content)?
        } else {
            AgentConfig::default()
        };

        Ok(Self {
            global,
            agent,
            workspace,
        })
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
pub struct RuntimeConfig {
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default = "default_history_messages")]
    pub history_messages: u32,
    #[serde(default)]
    pub summon_mode: SummonMode,
    #[serde(default = "default_true")]
    pub network_allow_all: bool,
    #[serde(default = "default_true")]
    pub read_allow_all: bool,
    #[serde(default = "default_safe_error_reply")]
    pub safe_error_reply: String,
    #[serde(default = "default_agent_workspace")]
    pub default_agent_workspace: PathBuf,
    #[serde(default)]
    pub summon_agents: HashMap<String, PathBuf>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            history_messages: default_history_messages(),
            summon_mode: SummonMode::default(),
            network_allow_all: true,
            read_allow_all: true,
            safe_error_reply: default_safe_error_reply(),
            default_agent_workspace: default_agent_workspace(),
            summon_agents: HashMap::new(),
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordInboundConfig {
    #[serde(default)]
    pub defaults: DiscordInboundPolicyConfig,
    #[serde(default)]
    pub servers: HashMap<String, DiscordServerInboundConfig>,
    #[serde(default)]
    pub dm: DiscordInboundPolicyConfig,
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
    pub allow_from: Option<Vec<String>>,
    #[serde(default)]
    pub require_mentions: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct DiscordInboundPolicy {
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
        if let Some(allow_from) = &lower.allow_from {
            self.allow_from = Some(allow_from.clone());
        }
        if let Some(require_mentions) = lower.require_mentions {
            self.require_mentions = Some(require_mentions);
        }
    }

    fn finalize(self, is_dm: bool) -> DiscordInboundPolicy {
        DiscordInboundPolicy {
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
pub struct ToolConfig {
    #[serde(default = "default_true")]
    pub memory: bool,
    #[serde(default = "default_true")]
    pub memorize: bool,
    #[serde(default = "default_true")]
    pub summon: bool,
    #[serde(default = "default_true")]
    pub search: bool,
    #[serde(default = "default_true")]
    pub clock: bool,
    #[serde(default = "default_true")]
    pub fetch: bool,
    #[serde(default = "default_true")]
    pub read: bool,
    #[serde(default = "default_true")]
    pub exec: bool,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            memory: true,
            memorize: true,
            summon: true,
            search: true,
            clock: true,
            fetch: true,
            read: true,
            exec: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_channel")]
    pub channel: ChannelKind,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            channel: default_channel(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Discord,
    Logging,
}

impl Default for ChannelKind {
    fn default() -> Self {
        Self::Discord
    }
}

fn default_agent_name() -> String {
    "default-agent".to_owned()
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
fn default_safe_error_reply() -> String {
    "I hit an internal error while processing that request.".to_owned()
}
fn default_agent_workspace() -> PathBuf {
    PathBuf::from("./workspace")
}
fn default_true() -> bool {
    true
}
fn default_channel() -> ChannelKind {
    ChannelKind::Discord
}
fn default_embedding_model() -> String {
    "all-MiniLM-L6-v2".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
            db_dir,
            logs_dir,
            run_dir,
        }
    }

    #[test]
    fn channel_policy_overrides_server_and_global() {
        let mut inbound = DiscordInboundConfig {
            defaults: DiscordInboundPolicyConfig {
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            ..DiscordInboundConfig::default()
        };

        inbound.servers.insert(
            "100".to_owned(),
            DiscordServerInboundConfig {
                policy: DiscordInboundPolicyConfig {
                    allow_from: Some(vec!["2".to_owned()]),
                    require_mentions: None,
                },
                channels: HashMap::from([(
                    "200".to_owned(),
                    DiscordInboundPolicyConfig {
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
        inbound.servers.insert(
            "100".to_owned(),
            DiscordServerInboundConfig {
                policy: DiscordInboundPolicyConfig {
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
                allow_from: None,
                require_mentions: Some(true),
            },
            dm: DiscordInboundPolicyConfig {
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
    fn rejects_unknown_channel_kind() {
        let yaml = r#"
channel: not_a_channel
"#;
        let parsed = serde_yaml::from_str::<RoutingConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn tool_config_missing_keys_default_to_enabled() {
        let parsed: ToolConfig = serde_yaml::from_str("memory: false\n").expect("valid yaml");
        assert!(!parsed.memory);
        assert!(parsed.memorize);
        assert!(parsed.summon);
        assert!(parsed.search);
        assert!(parsed.clock);
        assert!(parsed.fetch);
        assert!(parsed.read);
        assert!(parsed.exec);
    }

    #[test]
    fn agent_config_supports_memorize_and_exec_flags() {
        let parsed: AgentConfig = serde_yaml::from_str(
            r#"
name: custom
tools:
  memorize: false
  exec: false
"#,
        )
        .expect("valid yaml");
        assert_eq!(parsed.name, "custom");
        assert!(!parsed.tools.memorize);
        assert!(!parsed.tools.exec);
        assert!(parsed.tools.memory);
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
}
