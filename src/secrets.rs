use std::collections::HashMap;
use std::fs;

use crate::error::FrameworkError;
use crate::paths::AppPaths;

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
    use super::{SecretResolver, parse_secret_reference};
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
}
