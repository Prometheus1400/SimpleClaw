mod agents;
mod database;
mod defaults;
mod execution;
mod gateway;
mod normalize;
mod providers;
mod routing;
mod tools;
mod validate;

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::FrameworkError;
use crate::paths::AppPaths;
use crate::secrets::SecretResolver;
use crate::secrets::parse_secret_reference;

// ── Re-exports (preserves all consumer imports) ─────────────────────────────

pub use agents::{AgentEntryConfig, AgentInnerConfig, AgentsConfig};
pub use database::{DatabaseConfig, EmbeddingConfig};
#[allow(unused_imports)]
pub use execution::ExecutionDefaultsConfig;
#[allow(unused_imports)]
pub use execution::{AgentExecutionOverrides, MemoryRecallOverrides};
#[allow(unused_imports)]
pub use execution::{ExecutionConfig, LogLevel, MemoryRecallConfig, TransparencyConfig};
pub use gateway::{ChannelConfig, GatewayChannelKind, GatewayConfig};
pub use providers::{GeminiProviderConfig, ProviderEntryConfig, ProviderKind, ProvidersConfig};
#[allow(unused_imports)]
pub use routing::RoutingConfig;
#[allow(unused_imports)]
pub use tools::{
    ClockToolConfig, EditToolConfig, ExecToolConfig, ForgetToolConfig, MemorizeToolConfig,
    MemoryToolConfig, ProcessToolConfig, ReactToolConfig, ReadToolConfig, SkillsToolConfig,
    SummonToolConfig, TaskToolConfig, ToolSandboxConfig, ToolsConfig, WebFetchToolConfig,
    WebSearchProvider, WebSearchToolConfig,
};

// Re-exports used only by test code in other modules.
#[allow(unused_imports)]
pub use execution::SummonMode;
#[allow(unused_imports)]
pub use routing::{
    ChannelRoutingConfig, InboundPolicy, InboundPolicyConfig, WorkspaceRoutingConfig,
};

// ── Root config types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
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
        normalize::normalize_agents_workspace_paths(&mut global.agents);
        if let Some(workspace_override) = workspace_override {
            let workspace = normalize::normalize_workspace_path(workspace_override);
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
        validate::validate_providers_config(&global.providers)?;
        validate::validate_agents_config(&global.agents)?;
        validate::reconcile_routing_default_agent(&mut global.gateway.routing, &global.agents);
        validate::validate_gateway_config(&global.gateway)?;
        validate::validate_routing_config(&global.gateway.routing)?;

        Ok(Self { global })
    }
}

impl GlobalConfig {
    fn resolve_secrets(&mut self, paths: &AppPaths) -> Result<(), FrameworkError> {
        let resolver = SecretResolver::new(paths)?;
        self.providers.resolve_secrets(&resolver)?;
        for (kind, channel) in &mut self.gateway.channels {
            resolve_channel_secrets(kind, channel, &resolver)?;
        }
        for agent in &mut self.agents.list {
            resolve_agent_tool_secrets(agent, &resolver)?;
        }
        Ok(())
    }
}

fn resolve_agent_tool_secrets(
    agent: &mut AgentEntryConfig,
    resolver: &SecretResolver,
) -> Result<(), FrameworkError> {
    let Some(web_search) = agent.config.tools.web_search.as_mut() else {
        return Ok(());
    };
    let Some(raw) = web_search.api_key.as_deref() else {
        return Ok(());
    };

    let field_path = format!("agents.list[{}].config.tools.web_search.api_key", agent.id);
    let secret_name = parse_secret_reference(&field_path, raw)?;
    let value = resolver
        .resolve(&secret_name)
        .map_err(|err| FrameworkError::Config(format!("{field_path} failed to resolve: {err}")))?;
    web_search.api_key = Some(value);
    Ok(())
}

fn resolve_channel_secrets(
    kind: &GatewayChannelKind,
    channel: &mut gateway::ChannelConfig,
    resolver: &SecretResolver,
) -> Result<(), FrameworkError> {
    let Some(raw) = channel.token.as_deref() else {
        return Ok(());
    };
    let field_path = format!("gateway.channels.{}.token", kind.as_str());
    let secret_name = parse_secret_reference(&field_path, raw)?;
    let value = resolver
        .resolve(&secret_name)
        .map_err(|err| FrameworkError::Config(format!("{field_path} failed to resolve: {err}")))?;
    channel.token = Some(value);
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::paths::AppPaths;

    use normalize::{home_dir, normalize_workspace_path};
    use validate::{reconcile_routing_default_agent, validate_agents_config};

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
        let mut inbound = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["1".to_owned()]),
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };

        inbound.channels.insert(
            GatewayChannelKind::Discord,
            ChannelRoutingConfig {
                defaults: InboundPolicyConfig {
                    agent: Some("channel-kind".to_owned()),
                    allow_from: None,
                    require_mentions: None,
                },
                dm: InboundPolicyConfig::default(),
                workspaces: HashMap::from([(
                    "100".to_owned(),
                    WorkspaceRoutingConfig {
                        defaults: InboundPolicyConfig {
                            agent: Some("workspace".to_owned()),
                            allow_from: Some(vec!["2".to_owned()]),
                            require_mentions: None,
                        },
                        channels: HashMap::from([(
                            "200".to_owned(),
                            InboundPolicyConfig {
                                agent: Some("channel".to_owned()),
                                allow_from: Some(vec!["3".to_owned()]),
                                require_mentions: Some(false),
                            },
                        )]),
                    },
                )]),
            },
        );

        let policy = inbound.resolve(GatewayChannelKind::Discord, Some("100"), "200", false);
        assert_eq!(policy.agent, "channel");
        assert_eq!(policy.allow_from, Some(vec!["3".to_owned()]));
        assert!(!policy.require_mentions);
    }

    #[test]
    fn workspace_policy_applies_when_channel_missing() {
        let mut inbound = RoutingConfig::default();
        inbound.defaults.agent = Some("default".to_owned());
        inbound.channels.insert(
            GatewayChannelKind::Discord,
            ChannelRoutingConfig {
                defaults: InboundPolicyConfig {
                    agent: Some("channel-kind".to_owned()),
                    allow_from: None,
                    require_mentions: None,
                },
                dm: InboundPolicyConfig::default(),
                workspaces: HashMap::from([(
                    "100".to_owned(),
                    WorkspaceRoutingConfig {
                        defaults: InboundPolicyConfig {
                            agent: Some("workspace".to_owned()),
                            allow_from: Some(vec!["42".to_owned()]),
                            require_mentions: Some(true),
                        },
                        channels: HashMap::new(),
                    },
                )]),
            },
        );

        let policy = inbound.resolve(GatewayChannelKind::Discord, Some("100"), "201", false);
        assert_eq!(policy.agent, "workspace");
        assert_eq!(policy.allow_from, Some(vec!["42".to_owned()]));
        assert!(policy.require_mentions);
    }

    #[test]
    fn global_defaults_apply_when_no_workspace_or_channel_match() {
        let inbound = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: Some(vec!["9".to_owned()]),
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };

        let policy = inbound.resolve(GatewayChannelKind::Discord, Some("999"), "888", false);
        assert_eq!(policy.allow_from, Some(vec!["9".to_owned()]));
        assert!(policy.require_mentions);
    }

    #[test]
    fn dm_scope_overrides_defaults_and_forces_mentions_off() {
        let mut inbound = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };
        inbound.channels.insert(
            GatewayChannelKind::Discord,
            ChannelRoutingConfig {
                defaults: InboundPolicyConfig::default(),
                dm: InboundPolicyConfig {
                    agent: Some("dm".to_owned()),
                    allow_from: Some(vec!["11".to_owned()]),
                    require_mentions: Some(true),
                },
                workspaces: HashMap::new(),
            },
        );

        let policy = inbound.resolve(GatewayChannelKind::Discord, None, "321", true);
        assert_eq!(policy.agent, "dm");
        assert_eq!(policy.allow_from, Some(vec!["11".to_owned()]));
        assert!(!policy.require_mentions);
    }

    #[test]
    fn no_allow_from_defaults_to_allow_all() {
        let inbound = RoutingConfig::default();
        let policy = inbound.resolve(GatewayChannelKind::Discord, Some("100"), "200", false);
        assert!(policy.allows_user("123456789"));
    }

    #[test]
    fn dm_override_applies_after_channel_defaults() {
        let mut inbound = RoutingConfig {
            defaults: InboundPolicyConfig {
                agent: Some("default".to_owned()),
                allow_from: None,
                require_mentions: Some(true),
            },
            ..RoutingConfig::default()
        };

        inbound.channels.insert(
            GatewayChannelKind::Discord,
            ChannelRoutingConfig {
                defaults: InboundPolicyConfig {
                    agent: Some("server".to_owned()),
                    allow_from: Some(vec!["2".to_owned()]),
                    require_mentions: None,
                },
                dm: InboundPolicyConfig {
                    agent: Some("dm".to_owned()),
                    allow_from: Some(vec!["5".to_owned()]),
                    require_mentions: Some(true),
                },
                workspaces: HashMap::new(),
            },
        );

        let policy = inbound.resolve(GatewayChannelKind::Discord, None, "200", true);
        assert_eq!(policy.agent, "dm");
        assert_eq!(policy.allow_from, Some(vec!["5".to_owned()]));
        assert!(!policy.require_mentions);
    }

    #[test]
    fn execution_defaults_history_window() {
        let execution = ExecutionConfig::default();
        assert_eq!(execution.defaults.history_messages, 10);
        assert_eq!(execution.log_level, LogLevel::Info);
        assert!(execution.defaults.memory_recall.enabled);
        assert_eq!(execution.defaults.memory_recall.top_k, 3);
        assert!((execution.defaults.memory_recall.min_score - 0.72).abs() < f32::EPSILON);
    }

    #[test]
    fn execution_defaults_memory_recall_accepts_overrides() {
        let yaml = r#"
memory_recall:
  enabled: false
  top_k: 5
  min_score: 0.8
  long_term_weight: 0.7
  max_chars: 900
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml).expect("valid yaml");
        assert!(!parsed.memory_recall.enabled);
        assert_eq!(parsed.memory_recall.top_k, 5);
        assert!((parsed.memory_recall.min_score - 0.8).abs() < f32::EPSILON);
        assert!((parsed.memory_recall.long_term_weight - 0.7).abs() < f32::EPSILON);
        assert_eq!(parsed.memory_recall.max_chars, 900);
    }

    #[test]
    fn execution_defaults_memory_recall_rejects_unknown_fields() {
        let yaml = r#"
memory_recall:
  enabled: true
  bogus: 1
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn execution_defaults_rejects_legacy_memory_preinject_key() {
        let yaml = r#"
memory_preinject:
  enabled: true
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn execution_defaults_memory_recall_rejects_legacy_short_term_weight() {
        let yaml = r#"
memory_recall:
  short_term_weight: 0.3
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn runtime_memory_recall_normalized_clamps_values() {
        let config = MemoryRecallConfig {
            enabled: true,
            top_k: 999,
            min_score: 5.0,
            long_term_weight: -4.0,
            max_chars: 32,
        };
        let normalized = config.normalized();
        assert_eq!(normalized.top_k, 10);
        assert!((normalized.min_score - 1.0).abs() < f32::EPSILON);
        assert!((normalized.long_term_weight - 0.0).abs() < f32::EPSILON);
        assert_eq!(normalized.max_chars, 200);
    }

    #[test]
    fn execution_defaults_tool_call_transparency_defaults_off() {
        let execution = ExecutionConfig::default();
        assert!(!execution.defaults.transparency.tool_calls);
    }

    #[test]
    fn execution_defaults_tool_call_transparency_accepts_values() {
        let yaml = r#"
transparency:
  tool_calls: true
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml).expect("valid yaml");
        assert!(parsed.transparency.tool_calls);
    }

    #[test]
    fn execution_defaults_tool_call_transparency_rejects_unknown_value() {
        let yaml = r#"
transparency:
  tool_calls: true
  verbose: true
"#;
        let parsed = serde_yaml::from_str::<ExecutionDefaultsConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn execution_log_level_accepts_debug() {
        let yaml = r#"
log_level: debug
"#;
        let parsed = serde_yaml::from_str::<ExecutionConfig>(yaml).expect("valid yaml");
        assert_eq!(parsed.log_level, LogLevel::Debug);
    }

    #[test]
    fn execution_log_level_rejects_unknown_value() {
        let yaml = r#"
log_level: verbose
"#;
        let parsed = serde_yaml::from_str::<ExecutionConfig>(yaml);
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
        let parsed = serde_yaml::from_str::<ProviderEntryConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_legacy_provider_api_key_env_field() {
        let yaml = r#"
model: gemini-2.0-flash
api_base: https://example.com
api_key: "${secret:gemini_api_key}"
api_key_env: GEMINI_API_KEY
"#;
        let parsed = serde_yaml::from_str::<GeminiProviderConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_legacy_discord_token_env_field() {
        let yaml = r#"
token: "${secret:discord_token}"
token_env: DISCORD_TOKEN
"#;
        let parsed = serde_yaml::from_str::<ChannelConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_unknown_gateway_channel_kind() {
        let yaml = r#"
channels:
  not_a_channel:
    enabled: true
"#;
        let parsed = serde_yaml::from_str::<GatewayConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn tools_config_defaults_enable_all_builtin_tools() {
        let parsed: ToolsConfig = serde_yaml::from_str("{}\n").expect("valid yaml");
        assert_eq!(
            parsed.enabled_tool_names(),
            vec![
                "memory".to_owned(),
                "memorize".to_owned(),
                "forget".to_owned(),
                "summon".to_owned(),
                "task".to_owned(),
                "web_search".to_owned(),
                "clock".to_owned(),
                "react".to_owned(),
                "web_fetch".to_owned(),
                "read".to_owned(),
                "edit".to_owned(),
                "exec".to_owned(),
                "process".to_owned(),
            ]
        );
    }

    #[test]
    fn tools_config_rejects_legacy_enabled_tools_allowlist() {
        let parsed = serde_yaml::from_str::<ToolsConfig>("enabled_tools:\n  - memory\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_supports_typed_tool_map() {
        let parsed: AgentInnerConfig = serde_yaml::from_str(
            r#"
tools:
  read:
    enabled: false
  exec:
    enabled: true
    allow_background: false
    sandbox:
      enabled: true
      network_enabled: true
  skills:
    enabled: true
    ids:
      - code_review
      - release_checklist
"#,
        )
        .expect("valid yaml");
        assert!(!parsed.tools.read.expect("read config").enabled);
        let exec = parsed.tools.exec.expect("exec config");
        assert!(exec.enabled);
        assert!(!exec.allow_background);
        assert_eq!(exec.sandbox.network_enabled, Some(true));
        assert_eq!(
            parsed.tools.skills.expect("skills config").ids,
            vec!["code_review".to_owned(), "release_checklist".to_owned()]
        );
    }

    #[test]
    fn agent_config_defaults_tools_skills_config() {
        let parsed: AgentInnerConfig = serde_yaml::from_str("{}\n").expect("valid yaml");
        assert_eq!(parsed.tools.skills_config().ids, Vec::<String>::new());
    }

    #[test]
    fn agent_config_rejects_legacy_skills_field() {
        let parsed = serde_yaml::from_str::<AgentInnerConfig>(
            r#"
tools:
  skills:
    ids:
      - code_review
skills:
  enabled_skills:
    - code_review
"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_network_allow_all_field() {
        let parsed = serde_yaml::from_str::<AgentInnerConfig>("network_allow_all: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_read_allow_all_field() {
        let parsed = serde_yaml::from_str::<AgentInnerConfig>("read_allow_all: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_sandbox_field() {
        let parsed = serde_yaml::from_str::<AgentInnerConfig>("sandbox:\n  enabled: false\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn validate_agents_config_rejects_zero_tool_timeout() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig {
                    tools: ToolsConfig {
                        web_search: Some(WebSearchToolConfig {
                            enabled: true,
                            owner_restricted: true,
                            provider: WebSearchProvider::Duckduckgo,
                            api_key: None,
                            timeout_seconds: Some(0),
                        }),
                        ..ToolsConfig::default()
                    },
                    ..AgentInnerConfig::default()
                },
            }],
        };
        let result = validate_agents_config(&agents);
        assert!(result.is_err());
    }

    #[test]
    fn validate_agents_config_rejects_brave_without_api_key_when_enabled() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig {
                    tools: ToolsConfig {
                        web_search: Some(WebSearchToolConfig {
                            enabled: true,
                            owner_restricted: true,
                            provider: WebSearchProvider::Brave,
                            api_key: None,
                            timeout_seconds: Some(10),
                        }),
                        ..ToolsConfig::default()
                    },
                    ..AgentInnerConfig::default()
                },
            }],
        };
        let result = validate_agents_config(&agents);
        assert!(result.is_err());
    }

    #[test]
    fn validate_agents_config_rejects_duckduckgo_with_api_key() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig {
                    tools: ToolsConfig {
                        web_search: Some(WebSearchToolConfig {
                            enabled: true,
                            owner_restricted: true,
                            provider: WebSearchProvider::Duckduckgo,
                            api_key: Some("resolved-key".to_owned()),
                            timeout_seconds: Some(10),
                        }),
                        ..ToolsConfig::default()
                    },
                    ..AgentInnerConfig::default()
                },
            }],
        };
        let result = validate_agents_config(&agents);
        assert!(result.is_err());
    }

    #[test]
    fn validate_agents_config_rejects_zero_memory_top_k() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig {
                    tools: ToolsConfig {
                        memory: Some(MemoryToolConfig {
                            enabled: true,
                            owner_restricted: true,
                            default_top_k: Some(0),
                            max_top_k: None,
                        }),
                        ..ToolsConfig::default()
                    },
                    ..AgentInnerConfig::default()
                },
            }],
        };
        let result = validate_agents_config(&agents);
        assert!(result.is_err());
    }

    #[test]
    fn validate_agents_config_rejects_zero_web_fetch_max_chars() {
        let agents = AgentsConfig {
            default: "default".to_owned(),
            list: vec![AgentEntryConfig {
                id: "default".to_owned(),
                name: "Default".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig {
                    tools: ToolsConfig {
                        web_fetch: Some(WebFetchToolConfig {
                            enabled: true,
                            owner_restricted: true,
                            timeout_seconds: Some(10),
                            max_chars: Some(0),
                        }),
                        ..ToolsConfig::default()
                    },
                    ..AgentInnerConfig::default()
                },
            }],
        };
        let result = validate_agents_config(&agents);
        assert!(result.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_name_field() {
        let yaml = r#"
name: legacy
"#;
        let parsed = serde_yaml::from_str::<AgentInnerConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn agent_config_rejects_legacy_routing_field() {
        let yaml = r#"
routing:
  channel: discord
"#;
        let parsed = serde_yaml::from_str::<AgentInnerConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn global_config_resolves_required_secret_references() {
        let provider_env = "SIMPLECLAW_TEST_PROVIDER_SECRET";
        let discord_env = "SIMPLECLAW_TEST_DISCORD_SECRET";
        let web_search_env = "SIMPLECLAW_TEST_WEB_SEARCH_SECRET";
        unsafe {
            std::env::set_var(provider_env, "provider-secret");
            std::env::set_var(discord_env, "discord-secret");
            std::env::set_var(web_search_env, "web-search-secret");
        }

        let dir = unique_test_dir("resolve_refs");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig::default();
        global.gateway.channels.insert(
            GatewayChannelKind::Discord,
            ChannelConfig {
                token: Some(format!("${{secret:{discord_env}}}")),
                ..ChannelConfig::default()
            },
        );
        if let Some(ProviderEntryConfig::Gemini(provider)) =
            global.providers.entries.get_mut(&global.providers.default)
        {
            provider.api_key = Some(format!("${{secret:{provider_env}}}"));
        }
        global.agents.list[0].config.tools.web_search = Some(WebSearchToolConfig {
            enabled: true,
            owner_restricted: true,
            provider: WebSearchProvider::Brave,
            api_key: Some(format!("${{secret:{web_search_env}}}")),
            timeout_seconds: Some(20),
        });

        global
            .resolve_secrets(&paths)
            .expect("secret references should resolve");
        let api_key = match global.providers.entries.get(&global.providers.default) {
            Some(ProviderEntryConfig::Gemini(provider)) => provider.api_key.as_deref(),
            None => None,
        };
        assert_eq!(api_key, Some("provider-secret"));
        let channel = global
            .gateway
            .channels
            .get(&GatewayChannelKind::Discord)
            .expect("discord channel config should exist");
        assert_eq!(channel.token.as_deref(), Some("discord-secret"));
        assert_eq!(
            global.agents.list[0]
                .config
                .tools
                .web_search
                .as_ref()
                .and_then(|cfg| cfg.api_key.as_deref()),
            Some("web-search-secret")
        );

        unsafe {
            std::env::remove_var(provider_env);
            std::env::remove_var(discord_env);
            std::env::remove_var(web_search_env);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn global_config_rejects_plaintext_secret_values() {
        let dir = unique_test_dir("reject_plaintext");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig::default();
        global.gateway.channels.insert(
            GatewayChannelKind::Discord,
            ChannelConfig {
                token: Some("${secret:discord_token}".to_owned()),
                ..ChannelConfig::default()
            },
        );
        if let Some(ProviderEntryConfig::Gemini(provider)) =
            global.providers.entries.get_mut(&global.providers.default)
        {
            provider.api_key = Some("plaintext-key".to_owned());
        }
        global.agents.list[0].config.tools.web_search = Some(WebSearchToolConfig {
            enabled: true,
            owner_restricted: true,
            provider: WebSearchProvider::Brave,
            api_key: Some("plaintext-key".to_owned()),
            timeout_seconds: Some(20),
        });

        let err = global.resolve_secrets(&paths).unwrap_err();
        assert!(
            err.to_string()
                .contains("providers.entries.default.api_key must use secret reference syntax")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn global_config_rejects_plaintext_web_search_api_key() {
        let dir = unique_test_dir("reject_plaintext_web_search");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());

        let mut global = GlobalConfig::default();
        if let Some(ProviderEntryConfig::Gemini(provider)) =
            global.providers.entries.get_mut(&global.providers.default)
        {
            provider.api_key = None;
        }
        global.agents.list[0].config.tools.web_search = Some(WebSearchToolConfig {
            enabled: true,
            owner_restricted: true,
            provider: WebSearchProvider::Brave,
            api_key: Some("plaintext-key".to_owned()),
            timeout_seconds: Some(20),
        });

        let err = global.resolve_secrets(&paths).unwrap_err();
        assert!(err.to_string().contains(
            "agents.list[default].config.tools.web_search.api_key must use secret reference syntax"
        ));

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
        use normalize::normalize_agents_workspace_paths;

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
                    config: AgentInnerConfig::default(),
                },
                AgentEntryConfig {
                    id: "researcher".to_owned(),
                    name: "Researcher".to_owned(),
                    workspace: PathBuf::from("$SIMPLECLAW_TEST_SUMMON_ROOT/research"),
                    config: AgentInnerConfig::default(),
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
                workspace: PathBuf::from("/tmp/simpleclaw-summon-root/research"),
                config: AgentInnerConfig::default()
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
                    config: AgentInnerConfig::default(),
                },
                AgentEntryConfig {
                    id: "default".to_owned(),
                    name: "Duplicate".to_owned(),
                    workspace: PathBuf::from("./other"),
                    config: AgentInnerConfig::default(),
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
                config: AgentInnerConfig::default(),
            }],
        };
        assert!(validate_agents_config(&agents).is_err());
    }

    #[test]
    fn reconcile_routing_default_agent_uses_agents_default_when_legacy_default_missing() {
        let mut inbound = RoutingConfig::default();
        let agents = AgentsConfig {
            default: "researcher".to_owned(),
            list: vec![AgentEntryConfig {
                id: "researcher".to_owned(),
                name: "Researcher".to_owned(),
                workspace: PathBuf::from("./workspace"),
                config: AgentInnerConfig::default(),
            }],
        };

        reconcile_routing_default_agent(&mut inbound, &agents);
        assert_eq!(inbound.defaults.agent, Some("researcher".to_owned()));
    }

    #[test]
    fn reconcile_routing_default_agent_does_not_override_when_legacy_default_exists() {
        let mut inbound = RoutingConfig::default();
        let agents = AgentsConfig {
            default: "researcher".to_owned(),
            list: vec![
                AgentEntryConfig {
                    id: "default".to_owned(),
                    name: "Default".to_owned(),
                    workspace: PathBuf::from("./workspace"),
                    config: AgentInnerConfig::default(),
                },
                AgentEntryConfig {
                    id: "researcher".to_owned(),
                    name: "Researcher".to_owned(),
                    workspace: PathBuf::from("./workspace-researcher"),
                    config: AgentInnerConfig::default(),
                },
            ],
        };

        reconcile_routing_default_agent(&mut inbound, &agents);
        assert_eq!(inbound.defaults.agent, Some("default".to_owned()));
    }

    #[test]
    fn execution_config_rejects_legacy_workspace_fields() {
        let yaml = r#"
default_agent_workspace: ./workspace
summon_agents:
  researcher: ./agents/researcher
"#;
        let parsed = serde_yaml::from_str::<ExecutionConfig>(yaml);
        assert!(parsed.is_err());
    }

    #[test]
    fn execution_config_rejects_exec_policy_fields_moved_to_agent_config() {
        let yaml = r#"
network_allow_all: false
read_allow_all: false
sandbox: off
"#;
        let parsed = serde_yaml::from_str::<ExecutionConfig>(yaml);
        assert!(parsed.is_err());
    }
}
