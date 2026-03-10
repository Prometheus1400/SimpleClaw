use std::collections::HashMap;
use std::fmt;
use std::fs;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::FrameworkError;
use crate::paths::AppPaths;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T> {
    name: String,
    resolved: Option<T>,
}

impl<T> Secret<T> {
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            resolved: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn into_name(self) -> String {
        self.name
    }
}

impl Secret<String> {
    pub fn exposed(&self) -> Option<&str> {
        self.resolved.as_deref()
    }

    pub fn resolve(
        &mut self,
        resolver: &SecretResolver,
        field_path: &str,
    ) -> Result<(), FrameworkError> {
        let value = resolver.resolve(&self.name).map_err(|err| {
            FrameworkError::Config(format!("{field_path} failed to resolve: {err}"))
        })?;
        self.resolved = Some(value);
        Ok(())
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Serialize for Secret<String> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("${{secret:{}}}", self.name))
    }
}

impl<'de> Deserialize<'de> for Secret<String> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let name = parse_secret_reference("secret", &raw).map_err(serde::de::Error::custom)?;
        Ok(Self::from_name(name))
    }
}

pub struct SecretResolver {
    file_secrets: HashMap<String, String>,
    secrets_path_display: String,
}

impl SecretResolver {
    pub fn new(paths: &AppPaths) -> Result<Self, FrameworkError> {
        let mut file_secrets = HashMap::new();
        if paths.secrets_path.exists() {
            let content = fs::read_to_string(&paths.secrets_path)?;
            if !content.trim().is_empty() {
                file_secrets =
                    serde_yaml::from_str::<HashMap<String, String>>(&content).map_err(|err| {
                        FrameworkError::Config(format!(
                            "failed to parse secrets file {}: {err}",
                            paths.secrets_path.display()
                        ))
                    })?;
            }
        }

        Ok(Self {
            file_secrets,
            secrets_path_display: paths.secrets_path.display().to_string(),
        })
    }

    pub fn resolve(&self, name: &str) -> Result<String, FrameworkError> {
        if name.trim().is_empty() {
            return Err(FrameworkError::Config(
                "secret name cannot be empty".to_owned(),
            ));
        }

        if let Ok(value) = std::env::var(name) {
            return ensure_nonempty(
                value,
                format!("environment variable '{name}'"),
                name,
                &self.secrets_path_display,
            );
        }

        if let Some(value) = self.file_secrets.get(name) {
            return ensure_nonempty(
                value.clone(),
                format!("secrets file {}", self.secrets_path_display),
                name,
                &self.secrets_path_display,
            );
        }

        Err(FrameworkError::Config(format!(
            "secret '{name}' not found in environment variable '{name}' or secrets file {}",
            self.secrets_path_display
        )))
    }
}

pub fn parse_secret_reference(field_path: &str, raw: &str) -> Result<String, FrameworkError> {
    const PREFIX: &str = "${secret:";
    const SUFFIX: &str = "}";

    let trimmed = raw.trim();
    if !trimmed.starts_with(PREFIX) || !trimmed.ends_with(SUFFIX) {
        return Err(FrameworkError::Config(format!(
            "{field_path} must use secret reference syntax ${{secret:<name>}}"
        )));
    }

    let name = &trimmed[PREFIX.len()..trimmed.len() - SUFFIX.len()];
    if name.is_empty() || !is_valid_secret_name(name) {
        return Err(FrameworkError::Config(format!(
            "{field_path} has invalid secret reference '{trimmed}'; expected ${{secret:<name>}} with [A-Za-z0-9_.-]+"
        )));
    }

    Ok(name.to_owned())
}

fn is_valid_secret_name(name: &str) -> bool {
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

fn ensure_nonempty(
    value: String,
    source: String,
    name: &str,
    secrets_path_display: &str,
) -> Result<String, FrameworkError> {
    if value.trim().is_empty() {
        return Err(FrameworkError::Config(format!(
            "secret '{name}' resolved from {source} is empty; expected a non-empty value (searched env first, then {secrets_path_display})"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{Secret, SecretResolver, parse_secret_reference};
    use crate::paths::AppPaths;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{nanos}"))
    }

    fn test_paths(base_dir: std::path::PathBuf) -> AppPaths {
        let db_dir = base_dir.join("db");
        let logs_dir = base_dir.join("logs");
        let run_dir = base_dir.join("run");
        AppPaths {
            config_path: base_dir.join("config.yaml"),
            secrets_path: base_dir.join("secrets.yaml"),
            db_path: db_dir.join("short_term_memory.db"),
            long_term_db_path: db_dir.join("long_term_memory.db"),
            cron_db_path: db_dir.join("cron.db"),
            fastembed_cache_dir: base_dir.join(".fastembed_cache"),
            log_path: logs_dir.join("service.log"),
            pid_path: run_dir.join("service.pid"),
            base_dir,
            logs_dir,
            run_dir,
        }
    }

    #[test]
    fn parse_secret_reference_accepts_valid_pattern() {
        let parsed = parse_secret_reference("provider.api_key", "${secret:gemini_api_key}")
            .expect("valid reference should parse");
        assert_eq!(parsed, "gemini_api_key");
    }

    #[test]
    fn parse_secret_reference_rejects_plaintext() {
        let err = parse_secret_reference("provider.api_key", "raw-api-key").unwrap_err();
        assert!(err.to_string().contains("provider.api_key"));
    }

    #[test]
    fn parse_secret_reference_rejects_invalid_name() {
        let err = parse_secret_reference("provider.api_key", "${secret:bad name}").unwrap_err();
        assert!(err.to_string().contains("invalid secret reference"));
    }

    #[test]
    fn secret_deserialize_accepts_valid_pattern() {
        let secret: Secret<String> =
            serde_yaml::from_str("\"${secret:gemini_api_key}\"").expect("secret should parse");
        assert_eq!(secret.name(), "gemini_api_key");
        assert_eq!(secret.exposed(), None);
    }

    #[test]
    fn secret_deserialize_rejects_plaintext() {
        let err = serde_yaml::from_str::<Secret<String>>("\"plaintext\"").unwrap_err();
        assert!(err.to_string().contains("must use secret reference syntax"));
    }

    #[test]
    fn secret_serialize_preserves_reference_format() {
        let secret = Secret::<String>::from_name("gemini_api_key");
        let rendered = serde_yaml::to_string(&secret).expect("secret should serialize");
        assert!(rendered.contains("${secret:gemini_api_key}"));
    }

    #[test]
    fn resolver_prefers_env_over_file() {
        let key = "SIMPLECLAW_TEST_SECRET_RESOLVER_PREFERS_ENV";
        unsafe {
            std::env::set_var(key, "env-value");
        }

        let dir = unique_test_dir("resolver_env_file");
        fs::create_dir_all(&dir).expect("should create test dir");
        fs::write(dir.join("secrets.yaml"), format!("{key}: file-value\n"))
            .expect("should write test secrets file");
        let paths = test_paths(dir.clone());

        let resolver = SecretResolver::new(&paths).expect("resolver should load");
        let value = resolver.resolve(key).expect("secret should resolve");
        assert_eq!(value, "env-value");

        unsafe {
            std::env::remove_var(key);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolver_reads_file_when_env_missing() {
        let key = "simpleclaw_test_secret_file_only";
        let dir = unique_test_dir("resolver_file_only");
        fs::create_dir_all(&dir).expect("should create test dir");
        fs::write(dir.join("secrets.yaml"), format!("{key}: file-value\n"))
            .expect("should write test secrets file");
        let paths = test_paths(dir.clone());

        let resolver = SecretResolver::new(&paths).expect("resolver should load");
        let value = resolver.resolve(key).expect("secret should resolve");
        assert_eq!(value, "file-value");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolver_errors_when_missing_everywhere() {
        let key = "SIMPLECLAW_TEST_SECRET_DOES_NOT_EXIST";
        unsafe {
            std::env::remove_var(key);
        }

        let dir = unique_test_dir("resolver_missing");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());
        let resolver = SecretResolver::new(&paths).expect("resolver should load");

        let err = resolver.resolve(key).unwrap_err();
        assert!(err.to_string().contains("not found"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_resolve_populates_exposed_value() {
        let key = "SIMPLECLAW_TEST_SECRET_TYPED_RESOLVE";
        unsafe {
            std::env::set_var(key, "resolved");
        }

        let dir = unique_test_dir("secret_typed_resolve");
        fs::create_dir_all(&dir).expect("should create test dir");
        let paths = test_paths(dir.clone());
        let resolver = SecretResolver::new(&paths).expect("resolver should load");
        let mut secret = Secret::<String>::from_name(key);

        secret
            .resolve(&resolver, "providers.entries.default.api_key")
            .expect("typed secret should resolve");
        assert_eq!(secret.exposed(), Some("resolved"));

        unsafe {
            std::env::remove_var(key);
        }
        let _ = fs::remove_dir_all(dir);
    }
}
