use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::FrameworkError;
use crate::secrets::{Secret, SecretResolver, parse_secret_reference};

use super::defaults::{
    default_oauth_callback_path, default_oauth_redirect_host, default_oauth_redirect_port,
    default_oauth_timeout_secs, default_provider_api_base, default_provider_key,
    default_provider_model,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfig {
    pub default: String,
    pub entries: HashMap<String, ProviderEntryConfig>,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        let default_key = default_provider_key();
        let mut entries = HashMap::new();
        entries.insert(
            default_key.clone(),
            ProviderEntryConfig::Gemini(GeminiProviderConfig::default()),
        );
        Self {
            default: default_key,
            entries,
        }
    }
}

impl ProvidersConfig {
    pub(super) fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
    ) -> Result<(), FrameworkError> {
        for (key, entry) in &mut self.entries {
            entry.resolve_secrets(resolver, key)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEntryConfig {
    Gemini(GeminiProviderConfig),
    Moonshot(MoonshotProviderConfig),
}

impl ProviderEntryConfig {
    pub fn kind(&self) -> ProviderKind {
        match self {
            Self::Gemini(_) => ProviderKind::Gemini,
            Self::Moonshot(_) => ProviderKind::Moonshot,
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Gemini(config) => &config.model,
            Self::Moonshot(config) => &config.model,
        }
    }

    pub(super) fn validate(&self, key: &str) -> Result<(), FrameworkError> {
        match self {
            Self::Gemini(config) => config.validate(key),
            Self::Moonshot(config) => config.validate(key),
        }
    }

    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        match self {
            Self::Gemini(config) => config.resolve_secrets(resolver, key),
            Self::Moonshot(config) => config.resolve_secrets(resolver, key),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthMode {
    #[default]
    ApiKey,
    Oauth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthProviderConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub authorize_url: String,
    pub token_url: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default = "default_oauth_redirect_host")]
    pub redirect_host: String,
    #[serde(default = "default_oauth_redirect_port")]
    pub redirect_port: u16,
    #[serde(default = "default_oauth_callback_path")]
    pub callback_path: String,
    #[serde(default = "default_oauth_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for OAuthProviderConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: None,
            authorize_url: String::new(),
            token_url: String::new(),
            scopes: Vec::new(),
            redirect_host: default_oauth_redirect_host(),
            redirect_port: default_oauth_redirect_port(),
            callback_path: default_oauth_callback_path(),
            timeout_secs: default_oauth_timeout_secs(),
        }
    }
}

impl OAuthProviderConfig {
    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        resolve_secret_field(
            &mut self.client_secret,
            resolver,
            &format!("providers.entries.{key}.oauth.client_secret"),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeminiProviderConfig {
    #[serde(default = "default_provider_model")]
    pub model: String,
    #[serde(default = "default_provider_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub api_key: Option<Secret<String>>,
}

impl Default for GeminiProviderConfig {
    fn default() -> Self {
        Self {
            model: default_provider_model(),
            api_base: default_provider_api_base(),
            api_key: None,
        }
    }
}

impl GeminiProviderConfig {
    fn validate(&self, key: &str) -> Result<(), FrameworkError> {
        let prefix = format!("providers.entries.{key}");
        if self.model.trim().is_empty() {
            return Err(FrameworkError::Config(format!(
                "{prefix}.model must be non-empty"
            )));
        }
        if self.api_base.trim().is_empty() {
            return Err(FrameworkError::Config(format!(
                "{prefix}.api_base must be non-empty"
            )));
        }
        Ok(())
    }

    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        let Some(secret) = self.api_key.as_mut() else {
            return Ok(());
        };
        let path = format!("providers.entries.{key}.api_key");
        secret.resolve(resolver, &path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MoonshotProviderConfig {
    pub model: String,
    pub api_base: String,
    #[serde(default)]
    pub mode: ProviderAuthMode,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub oauth: Option<OAuthProviderConfig>,
}

impl Default for MoonshotProviderConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            api_base: String::new(),
            mode: ProviderAuthMode::default(),
            api_key: None,
            oauth: None,
        }
    }
}

impl MoonshotProviderConfig {
    fn validate(&self, key: &str) -> Result<(), FrameworkError> {
        validate_moonshot_provider(
            self.mode,
            &self.model,
            &self.api_base,
            self.api_key.as_deref(),
            self.oauth.as_ref(),
            key,
        )
    }

    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        resolve_secret_field(
            &mut self.api_key,
            resolver,
            &format!("providers.entries.{key}.api_key"),
        )?;
        if let Some(oauth) = &mut self.oauth {
            oauth.resolve_secrets(resolver, key)?;
        }
        Ok(())
    }
}

fn validate_moonshot_provider(
    mode: ProviderAuthMode,
    model: &str,
    api_base: &str,
    api_key: Option<&str>,
    _oauth: Option<&OAuthProviderConfig>,
    key: &str,
) -> Result<(), FrameworkError> {
    let prefix = format!("providers.entries.{key}");
    if model.trim().is_empty() {
        return Err(FrameworkError::Config(format!(
            "{prefix}.model must be non-empty"
        )));
    }
    if api_base.trim().is_empty() {
        return Err(FrameworkError::Config(format!(
            "{prefix}.api_base must be non-empty"
        )));
    }
    match mode {
        ProviderAuthMode::ApiKey => {
            let Some(api_key) = api_key else {
                return Err(FrameworkError::Config(format!(
                    "{prefix}.api_key is required when mode is api_key"
                )));
            };
            if api_key.trim().is_empty() {
                return Err(FrameworkError::Config(format!(
                    "{prefix}.api_key must be non-empty when mode is api_key"
                )));
            }
        }
        ProviderAuthMode::Oauth => {
            return Err(FrameworkError::Config(format!(
                "{prefix}.mode oauth is not supported for kind moonshot"
            )));
        }
    }
    Ok(())
}

fn resolve_secret_field(
    value: &mut Option<String>,
    resolver: &SecretResolver,
    path: &str,
) -> Result<(), FrameworkError> {
    let Some(raw) = value.as_deref() else {
        return Ok(());
    };
    let secret_name = parse_secret_reference(path, raw)?;
    let resolved = resolver
        .resolve(&secret_name)
        .map_err(|err| FrameworkError::Config(format!("{path} failed to resolve: {err}")))?;
    *value = Some(resolved);
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Gemini,
    Moonshot,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Moonshot => "moonshot",
        }
    }
}
