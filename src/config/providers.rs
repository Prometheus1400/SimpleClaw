use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::FrameworkError;
use crate::secrets::{SecretResolver, parse_secret_reference};

use super::defaults::{default_provider_api_base, default_provider_key, default_provider_model};

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
}

impl ProviderEntryConfig {
    pub fn kind(&self) -> ProviderKind {
        match self {
            Self::Gemini(_) => ProviderKind::Gemini,
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Gemini(config) => &config.model,
        }
    }

    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        match self {
            Self::Gemini(config) => config.resolve_secrets(resolver, key),
        }
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
    pub api_key: Option<String>,
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
    fn resolve_secrets(
        &mut self,
        resolver: &SecretResolver,
        key: &str,
    ) -> Result<(), FrameworkError> {
        let Some(raw) = self.api_key.as_deref() else {
            return Ok(());
        };
        let path = format!("providers.entries.{key}.api_key");
        let secret_name = parse_secret_reference(&path, raw)?;
        let value = resolver
            .resolve(&secret_name)
            .map_err(|err| FrameworkError::Config(format!("{path} failed to resolve: {err}")))?;
        self.api_key = Some(value);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Gemini,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
        }
    }
}
