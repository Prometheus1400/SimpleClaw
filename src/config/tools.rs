use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::FrameworkError;
use crate::secrets::Secret;

fn default_enabled() -> bool {
    true
}

fn default_owner_restricted() -> bool {
    true
}

fn default_cron_max_jobs_per_agent() -> Option<u32> {
    Some(50)
}

fn default_cron_guard_timeout_seconds() -> Option<u64> {
    Some(10)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub read: Option<ReadToolConfig>,
    pub edit: Option<EditToolConfig>,
    pub glob: Option<GlobToolConfig>,
    pub grep: Option<GrepToolConfig>,
    pub list: Option<ListToolConfig>,
    pub exec: Option<ExecToolConfig>,
    pub background: Option<BackgroundToolConfig>,
    pub wait: Option<WaitToolConfig>,
    pub web_search: Option<WebSearchToolConfig>,
    pub web_fetch: Option<WebFetchToolConfig>,
    pub memory: Option<MemoryToolConfig>,
    pub memorize: Option<MemorizeToolConfig>,
    pub forget: Option<ForgetToolConfig>,
    pub summon: Option<SummonToolConfig>,
    pub task: Option<TaskToolConfig>,
    pub clock: Option<ClockToolConfig>,
    pub cron: Option<CronToolConfig>,
    pub react: Option<ReactToolConfig>,
    pub skills: Option<SkillsToolConfig>,
}

impl ToolsConfig {
    pub fn enabled_tool_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.memory.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("memory".to_owned());
        }
        if self
            .memorize
            .as_ref()
            .map(|cfg| cfg.enabled)
            .unwrap_or(true)
        {
            names.push("memorize".to_owned());
        }
        if self.forget.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("forget".to_owned());
        }
        if self.summon.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("summon".to_owned());
        }
        if self.task.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("task".to_owned());
        }
        if self
            .web_search
            .as_ref()
            .map(|cfg| cfg.enabled)
            .unwrap_or(true)
        {
            names.push("web_search".to_owned());
        }
        if self.clock.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("clock".to_owned());
        }
        if self.cron.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("cron".to_owned());
        }
        if self.react.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("react".to_owned());
        }
        if self
            .web_fetch
            .as_ref()
            .map(|cfg| cfg.enabled)
            .unwrap_or(true)
        {
            names.push("web_fetch".to_owned());
        }
        if self.read.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("read".to_owned());
        }
        if self.edit.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("edit".to_owned());
        }
        if self.glob.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("glob".to_owned());
        }
        if self.grep.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("grep".to_owned());
        }
        if self.list.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("list".to_owned());
        }
        if self.exec.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("exec".to_owned());
        }
        if self
            .background
            .as_ref()
            .map(|cfg| cfg.enabled)
            .unwrap_or(true)
        {
            names.push("background".to_owned());
        }
        if self.wait.as_ref().map(|cfg| cfg.enabled).unwrap_or(true) {
            names.push("wait".to_owned());
        }
        names
    }

    pub fn config_for_tool(&self, name: &str) -> Result<Option<Value>, FrameworkError> {
        let value = match name {
            "read" => serde_json::to_value(self.read.clone().unwrap_or_default()).ok(),
            "edit" => serde_json::to_value(self.edit.clone().unwrap_or_default()).ok(),
            "glob" => serde_json::to_value(self.glob.clone().unwrap_or_default()).ok(),
            "grep" => serde_json::to_value(self.grep.clone().unwrap_or_default()).ok(),
            "list" => serde_json::to_value(self.list.clone().unwrap_or_default()).ok(),
            "exec" => serde_json::to_value(self.exec.clone().unwrap_or_default()).ok(),
            "background" => serde_json::to_value(self.background.clone().unwrap_or_default()).ok(),
            "wait" => serde_json::to_value(self.wait.clone().unwrap_or_default()).ok(),
            "web_search" => Some(
                serde_json::to_value(self.web_search.clone().unwrap_or_default().to_runtime()?)
                    .map_err(|err| {
                        FrameworkError::Config(format!(
                            "failed to serialize runtime web_search config: {err}"
                        ))
                    })?,
            ),
            "web_fetch" => serde_json::to_value(self.web_fetch.clone().unwrap_or_default()).ok(),
            "memory" => serde_json::to_value(self.memory.clone().unwrap_or_default()).ok(),
            "memorize" => serde_json::to_value(self.memorize.clone().unwrap_or_default()).ok(),
            "forget" => serde_json::to_value(self.forget.clone().unwrap_or_default()).ok(),
            "summon" => serde_json::to_value(self.summon.clone().unwrap_or_default()).ok(),
            "task" => serde_json::to_value(self.task.clone().unwrap_or_default()).ok(),
            "clock" => serde_json::to_value(self.clock.clone().unwrap_or_default()).ok(),
            "cron" => serde_json::to_value(self.cron.clone().unwrap_or_default()).ok(),
            "react" => serde_json::to_value(self.react.clone().unwrap_or_default()).ok(),
            "skills" => serde_json::to_value(self.skills.clone().unwrap_or_default()).ok(),
            _ => None,
        };
        Ok(value)
    }

    pub fn owner_restricted_for_tool(&self, name: &str) -> Option<bool> {
        let owner_restricted = match name {
            "read" => self.read.clone().unwrap_or_default().owner_restricted,
            "edit" => self.edit.clone().unwrap_or_default().owner_restricted,
            "glob" => self.glob.clone().unwrap_or_default().owner_restricted,
            "grep" => self.grep.clone().unwrap_or_default().owner_restricted,
            "list" => self.list.clone().unwrap_or_default().owner_restricted,
            "exec" => self.exec.clone().unwrap_or_default().owner_restricted,
            "background" => self.background.clone().unwrap_or_default().owner_restricted,
            "wait" => self.wait.clone().unwrap_or_default().owner_restricted,
            "web_search" => self.web_search.clone().unwrap_or_default().owner_restricted,
            "web_fetch" => self.web_fetch.clone().unwrap_or_default().owner_restricted,
            "memory" => self.memory.clone().unwrap_or_default().owner_restricted,
            "memorize" => self.memorize.clone().unwrap_or_default().owner_restricted,
            "forget" => self.forget.clone().unwrap_or_default().owner_restricted,
            "summon" => self.summon.clone().unwrap_or_default().owner_restricted,
            "task" => self.task.clone().unwrap_or_default().owner_restricted,
            "clock" => self.clock.clone().unwrap_or_default().owner_restricted,
            "cron" => self.cron.clone().unwrap_or_default().owner_restricted,
            "react" => self.react.clone().unwrap_or_default().owner_restricted,
            _ => return None,
        };
        Some(owner_restricted)
    }

    pub fn with_disabled(&self, names: &[&str]) -> Self {
        let mut next = self.clone();
        for name in names {
            match *name {
                "read" => next.read.get_or_insert_with(Default::default).enabled = false,
                "edit" => next.edit.get_or_insert_with(Default::default).enabled = false,
                "glob" => next.glob.get_or_insert_with(Default::default).enabled = false,
                "grep" => next.grep.get_or_insert_with(Default::default).enabled = false,
                "list" => next.list.get_or_insert_with(Default::default).enabled = false,
                "exec" => next.exec.get_or_insert_with(Default::default).enabled = false,
                "background" => {
                    next.background.get_or_insert_with(Default::default).enabled = false
                }
                "wait" => next.wait.get_or_insert_with(Default::default).enabled = false,
                "web_search" => {
                    next.web_search.get_or_insert_with(Default::default).enabled = false
                }
                "web_fetch" => next.web_fetch.get_or_insert_with(Default::default).enabled = false,
                "memory" => next.memory.get_or_insert_with(Default::default).enabled = false,
                "memorize" => next.memorize.get_or_insert_with(Default::default).enabled = false,
                "forget" => next.forget.get_or_insert_with(Default::default).enabled = false,
                "summon" => next.summon.get_or_insert_with(Default::default).enabled = false,
                "task" => next.task.get_or_insert_with(Default::default).enabled = false,
                "clock" => next.clock.get_or_insert_with(Default::default).enabled = false,
                "cron" => next.cron.get_or_insert_with(Default::default).enabled = false,
                "react" => next.react.get_or_insert_with(Default::default).enabled = false,
                "skills" => next.skills.get_or_insert_with(Default::default).enabled = false,
                _ => {}
            }
        }
        next
    }

    pub fn skills_config(&self) -> SkillsToolConfig {
        self.skills.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSandboxConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub extra_readable_paths: Vec<String>,
    pub extra_writable_paths: Vec<String>,
    pub network_enabled: Option<bool>,
}

impl Default for ToolSandboxConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            extra_readable_paths: Vec::new(),
            extra_writable_paths: Vec::new(),
            network_enabled: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ReadToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for ReadToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EditToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for EditToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GlobToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for GlobToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GrepToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for GrepToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ListToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for ListToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExecToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub allow_background: bool,
    pub sandbox: ToolSandboxConfig,
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            allow_background: true,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BackgroundToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
}

impl Default for BackgroundToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WaitToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
}

impl Default for WaitToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    #[serde(default)]
    pub provider: WebSearchProvider,
    #[serde(default)]
    pub api_key: Option<Secret<String>>,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchToolRuntimeConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    #[serde(default)]
    pub provider: WebSearchProvider,
    pub api_key: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for WebSearchToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            provider: WebSearchProvider::default(),
            api_key: None,
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

impl Default for WebSearchToolRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            provider: WebSearchProvider::default(),
            api_key: None,
            timeout_seconds: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

impl WebSearchToolConfig {
    pub fn to_runtime(&self) -> Result<WebSearchToolRuntimeConfig, FrameworkError> {
        let api_key = self
            .api_key
            .as_ref()
            .map(|secret| {
                secret.exposed().map(str::to_owned).ok_or_else(|| {
                    FrameworkError::Config(
                        "web_search api_key was not resolved before runtime assembly".to_owned(),
                    )
                })
            })
            .transpose()?;

        Ok(WebSearchToolRuntimeConfig {
            enabled: self.enabled,
            owner_restricted: self.owner_restricted,
            provider: self.provider,
            api_key,
            timeout_seconds: self.timeout_seconds,
            sandbox: self.sandbox.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchProvider {
    Brave,
    #[default]
    Duckduckgo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub timeout_seconds: Option<u64>,
    pub max_chars: Option<u32>,
    pub sandbox: ToolSandboxConfig,
}

impl Default for WebFetchToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            timeout_seconds: None,
            max_chars: None,
            sandbox: ToolSandboxConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub default_top_k: Option<u32>,
    pub max_top_k: Option<u32>,
}

impl Default for MemoryToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            default_top_k: None,
            max_top_k: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MemorizeToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
}

impl Default for MemorizeToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ForgetToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
}

impl Default for ForgetToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SummonToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub allow_background: bool,
    pub allowed: Vec<String>,
}

impl Default for SummonToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            allow_background: true,
            allowed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    pub allow_background: bool,
    pub worker_max_steps: Option<u32>,
}

impl Default for TaskToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            allow_background: true,
            worker_max_steps: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ClockToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
}

impl Default for ClockToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CronToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_owner_restricted")]
    pub owner_restricted: bool,
    #[serde(default = "default_cron_max_jobs_per_agent")]
    pub max_jobs_per_agent: Option<u32>,
    #[serde(default = "default_cron_guard_timeout_seconds")]
    pub guard_timeout_seconds: Option<u64>,
}

impl Default for CronToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: default_owner_restricted(),
            max_jobs_per_agent: default_cron_max_jobs_per_agent(),
            guard_timeout_seconds: default_cron_guard_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ReactToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub owner_restricted: bool,
}

impl Default for ReactToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            owner_restricted: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsToolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub disabled_skills: Vec<String>,
}

impl Default for SkillsToolConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            disabled_skills: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SummonToolConfig, TaskToolConfig, ToolSandboxConfig, WebSearchProvider, WebSearchToolConfig,
    };
    use crate::secrets::Secret;

    #[test]
    fn web_search_to_runtime_rejects_unresolved_secret() {
        let config = WebSearchToolConfig {
            enabled: true,
            owner_restricted: true,
            provider: WebSearchProvider::Brave,
            api_key: Some(Secret::from_name("BRAVE_API_KEY")),
            timeout_seconds: Some(20),
            sandbox: ToolSandboxConfig::default(),
        };

        let err = config
            .to_runtime()
            .expect_err("unresolved secret should fail runtime assembly");
        assert!(
            err.to_string()
                .contains("web_search api_key was not resolved before runtime assembly")
        );
    }

    #[test]
    fn summon_and_task_allow_background_default_to_true() {
        assert!(SummonToolConfig::default().allow_background);
        assert!(TaskToolConfig::default().allow_background);
    }
}
