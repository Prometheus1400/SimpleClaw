use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrameworkError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("tool error: {0}")]
    Tool(String),

    #[error("configuration error: {0}")]
    Config(String),
}
