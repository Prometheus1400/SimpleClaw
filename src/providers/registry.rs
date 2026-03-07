use std::collections::HashMap;
use std::sync::Arc;

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
    fn create(&self, entry: &ProviderEntryConfig) -> Result<Arc<dyn Provider>, FrameworkError>;
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
    ) -> Result<Arc<dyn Provider>, FrameworkError> {
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

    fn create(&self, entry: &ProviderEntryConfig) -> Result<Arc<dyn Provider>, FrameworkError> {
        let provider = GeminiProvider::from_entry(entry)?;
        Ok(Arc::new(provider))
    }
}
