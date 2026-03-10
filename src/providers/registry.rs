use std::collections::HashMap;
use std::sync::Arc;

use crate::config::ProvidersConfig;
use crate::config::{ProviderEntryConfig, ProviderKind};
use crate::error::FrameworkError;

use super::gemini::GeminiProvider;
use super::moonshot_compatible::MoonshotCompatibleProvider;
use super::openai_codex::OpenAiCodexProvider;
use super::types::Provider;

pub struct ProviderMetadata {
    pub kind: ProviderKind,
    pub supports_native_tools: bool,
    pub known_models: &'static [&'static str],
}

pub trait ProviderAdapter: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;
    fn create(
        &self,
        provider_key: &str,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError>;
}

pub struct ProviderRegistry {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        let mut adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>> = HashMap::new();
        adapters.insert(ProviderKind::Gemini, Arc::new(GeminiProviderAdapter));
        adapters.insert(ProviderKind::Moonshot, Arc::new(MoonshotProviderAdapter));
        adapters.insert(
            ProviderKind::OpenAiCodex,
            Arc::new(OpenAiCodexProviderAdapter),
        );
        Self { adapters }
    }

    pub fn create_provider(
        &self,
        provider_key: &str,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError> {
        let kind = entry.kind();
        let Some(adapter) = self.adapters.get(&kind) else {
            return Err(FrameworkError::Config(format!(
                "provider kind '{}' is not registered",
                kind.as_str()
            )));
        };
        adapter.create(provider_key, entry)
    }

    pub fn metadata_for_kind(
        &self,
        kind: ProviderKind,
    ) -> Result<ProviderMetadata, FrameworkError> {
        let Some(adapter) = self.adapters.get(&kind) else {
            return Err(FrameworkError::Config(format!(
                "provider kind '{}' is not registered",
                kind.as_str()
            )));
        };
        Ok(adapter.metadata())
    }
}

pub struct ProviderFactory {
    entries: HashMap<String, ProviderEntry>,
}

pub struct ProviderEntry {
    provider: Box<dyn Provider>,
    supports_native_tools: bool,
}

impl ProviderFactory {
    pub fn from_config(config: &ProvidersConfig) -> Result<Self, FrameworkError> {
        let registry = ProviderRegistry::new();
        let mut entries = HashMap::new();
        for (key, entry) in &config.entries {
            let provider = registry.create_provider(key, entry)?;
            let metadata = registry.metadata_for_kind(entry.kind())?;
            entries.insert(
                key.clone(),
                ProviderEntry {
                    provider,
                    supports_native_tools: metadata.supports_native_tools,
                },
            );
        }
        Ok(Self { entries })
    }

    pub fn from_parts(entries: HashMap<String, (Box<dyn Provider>, bool)>) -> Self {
        let entries = entries
            .into_iter()
            .map(|(key, (provider, supports_native_tools))| {
                (
                    key,
                    ProviderEntry {
                        provider,
                        supports_native_tools,
                    },
                )
            })
            .collect();
        Self { entries }
    }

    pub fn get(&self, key: &str) -> Result<&dyn Provider, FrameworkError> {
        let Some(entry) = self.entries.get(key) else {
            return Err(FrameworkError::Config(format!(
                "unknown provider key '{key}'"
            )));
        };
        Ok(entry.provider.as_ref())
    }

    pub fn supports_native_tools(&self, key: &str) -> bool {
        self.entries
            .get(key)
            .map(|entry| entry.supports_native_tools)
            .unwrap_or(false)
    }
}

struct GeminiProviderAdapter;

impl ProviderAdapter for GeminiProviderAdapter {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            kind: ProviderKind::Gemini,
            supports_native_tools: true,
            known_models: &[
                "gemini-2.5-flash",
                "gemini-2.5-pro",
                "gemini-2.0-flash",
                "gemini-2.0-flash-lite",
            ],
        }
    }

    fn create(
        &self,
        _provider_key: &str,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError> {
        let provider = GeminiProvider::from_entry(entry)?;
        Ok(Box::new(provider))
    }
}

struct MoonshotProviderAdapter;

impl ProviderAdapter for MoonshotProviderAdapter {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            kind: ProviderKind::Moonshot,
            supports_native_tools: true,
            known_models: &[],
        }
    }

    fn create(
        &self,
        provider_key: &str,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError> {
        let ProviderEntryConfig::Moonshot(config) = entry else {
            return Err(FrameworkError::Config(
                "moonshot provider adapter received wrong provider config variant".to_owned(),
            ));
        };
        let provider =
            MoonshotCompatibleProvider::from_moonshot_config(provider_key, config.clone())?;
        Ok(Box::new(provider))
    }
}

struct OpenAiCodexProviderAdapter;

impl ProviderAdapter for OpenAiCodexProviderAdapter {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            kind: ProviderKind::OpenAiCodex,
            supports_native_tools: true,
            known_models: &["gpt-5.3-codex", "gpt-5-codex", "gpt-5.1-codex-mini"],
        }
    }

    fn create(
        &self,
        _provider_key: &str,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError> {
        let ProviderEntryConfig::OpenAiCodex(config) = entry else {
            return Err(FrameworkError::Config(
                "openai_codex provider adapter received wrong provider config variant".to_owned(),
            ));
        };
        let provider = OpenAiCodexProvider::from_config(config.clone())?;
        Ok(Box::new(provider))
    }
}
