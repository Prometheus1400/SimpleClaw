use std::env;
use std::fs;
use std::path::PathBuf;

use crate::error::FrameworkError;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub base_dir: PathBuf,
    pub config_path: PathBuf,
    pub secrets_path: PathBuf,
    pub db_path: PathBuf,
    pub long_term_db_path: PathBuf,
    pub session_db_path: PathBuf,
    pub cron_db_path: PathBuf,
    pub fastembed_cache_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub log_path: PathBuf,
    pub run_dir: PathBuf,
    pub pid_path: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, FrameworkError> {
        let home = home_dir().ok_or_else(|| {
            FrameworkError::Config(
                "failed to resolve home directory; cannot resolve ~/.simpleclaw".to_owned(),
            )
        })?;
        let base_dir = home.join(".simpleclaw");
        let db_dir = base_dir.join("db");
        let logs_dir = base_dir.join("logs");
        let run_dir = base_dir.join("run");
        Ok(Self {
            config_path: base_dir.join("config.yaml"),
            secrets_path: base_dir.join("secrets.yaml"),
            db_path: db_dir.join("short_term_memory.db"),
            long_term_db_path: db_dir.join("long_term_memory.db"),
            session_db_path: db_dir.join("sessions.db"),
            cron_db_path: db_dir.join("cron.db"),
            fastembed_cache_dir: base_dir.join(".fastembed_cache"),
            log_path: logs_dir.join("service.log"),
            pid_path: run_dir.join("service.pid"),
            base_dir,
            logs_dir,
            run_dir,
        })
    }

    pub fn ensure_base_dir(&self) -> Result<(), FrameworkError> {
        fs::create_dir_all(&self.base_dir)?;
        Ok(())
    }

    pub fn ensure_runtime_dirs(&self) -> Result<(), FrameworkError> {
        fs::create_dir_all(&self.logs_dir)?;
        fs::create_dir_all(&self.run_dir)?;
        Ok(())
    }

    pub fn ensure_fastembed_cache_dir(&self) -> Result<(), FrameworkError> {
        fs::create_dir_all(&self.fastembed_cache_dir)?;
        Ok(())
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::AppPaths;

    #[test]
    fn resolve_has_expected_layout() {
        let paths = AppPaths::resolve().expect("home directory should be available in test env");
        assert_eq!(
            paths.base_dir.file_name().and_then(|s| s.to_str()),
            Some(".simpleclaw")
        );
        assert_eq!(
            paths.config_path.file_name().and_then(|s| s.to_str()),
            Some("config.yaml")
        );
        assert_eq!(
            paths.secrets_path.file_name().and_then(|s| s.to_str()),
            Some("secrets.yaml")
        );
        let db_dir_name = paths
            .db_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str());
        assert_eq!(db_dir_name, Some("db"));
        assert_eq!(
            paths.db_path.file_name().and_then(|s| s.to_str()),
            Some("short_term_memory.db")
        );
        assert_eq!(
            paths.long_term_db_path.file_name().and_then(|s| s.to_str()),
            Some("long_term_memory.db")
        );
        assert_eq!(
            paths.session_db_path.file_name().and_then(|s| s.to_str()),
            Some("sessions.db")
        );
        assert_eq!(
            paths.cron_db_path.file_name().and_then(|s| s.to_str()),
            Some("cron.db")
        );
        assert_eq!(
            paths.logs_dir.file_name().and_then(|s| s.to_str()),
            Some("logs")
        );
        assert_eq!(
            paths
                .fastembed_cache_dir
                .file_name()
                .and_then(|s| s.to_str()),
            Some(".fastembed_cache")
        );
        assert_eq!(
            paths.run_dir.file_name().and_then(|s| s.to_str()),
            Some("run")
        );
    }
}
