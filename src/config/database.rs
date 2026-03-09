use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::defaults::{
    default_busy_timeout_ms, default_db_path, default_embedding_model, default_long_term_db_path,
    default_pool_size,
};

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
