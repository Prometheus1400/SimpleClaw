use std::collections::HashMap;
use std::sync::Arc;

use crate::config::ProvidersConfig;
use crate::config::{ProviderEntryConfig, ProviderKind};
use crate::error::FrameworkError;

use super::gemini::GeminiProvider;
use super::types::Provider;

pub struct ProviderMetadata {
    pub kind: ProviderKind,
    pub supports_native_tools: bool,
    pub known_models: &'static [&'static str],
}

pub trait ProviderAdapter: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;
    fn create(&self, entry: &ProviderEntryConfig) -> Result<Box<dyn Provider>, FrameworkError>;
}

pub struct ProviderRegistry {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        let mut adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>> = HashMap::new();
        adapters.insert(ProviderKind::Gemini, Arc::new(GeminiProviderAdapter));
        Self { adapters }
    }

    pub fn create_provider(
        &self,
        entry: &ProviderEntryConfig,
    ) -> Result<Box<dyn Provider>, FrameworkError> {
        let kind = entry.kind();
        let Some(adapter) = self.adapters.get(&kind) else {
            return Err(FrameworkError::Config(format!(
                "provider kind '{}' is not registered",
                kind.as_str()
            )));
        };
        adapter.create(entry)
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
            let provider = registry.create_provider(entry)?;
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

    fn create(&self, entry: &ProviderEntryConfig) -> Result<Box<dyn Provider>, FrameworkError> {
        let provider = GeminiProvider::from_entry(entry)?;
        Ok(Box::new(provider))
    }
}
