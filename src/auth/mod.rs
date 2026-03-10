mod openai_oauth;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::FrameworkError;
use crate::paths::AppPaths;

pub(crate) use openai_oauth::extract_account_id_from_jwt;

pub(crate) const OPENAI_CODEX_PROVIDER: &str = "openai_codex";

const AUTH_SCHEMA_VERSION: u32 = 1;
const OPENAI_REFRESH_SKEW_SECS: i64 = 90;
const DEFAULT_PROFILE_NAME: &str = "default";
const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TokenSet {
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) id_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_at_unix: Option<i64>,
    #[serde(default)]
    pub(crate) token_type: Option<String>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

impl TokenSet {
    fn is_expiring_within(&self, skew: i64) -> bool {
        let Some(expires_at_unix) = self.expires_at_unix else {
            return false;
        };
        now_unix().saturating_add(skew) >= expires_at_unix
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthProfile {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) profile_name: String,
    #[serde(default)]
    pub(crate) account_id: Option<String>,
    pub(crate) token_set: TokenSet,
    #[serde(default = "now_unix")]
    pub(crate) created_at_unix: i64,
    #[serde(default = "now_unix")]
    pub(crate) updated_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthData {
    #[serde(default = "default_auth_schema_version")]
    schema_version: u32,
    #[serde(default = "now_unix")]
    updated_at_unix: i64,
    #[serde(default)]
    active_profiles: BTreeMap<String, String>,
    #[serde(default)]
    profiles: BTreeMap<String, AuthProfile>,
}

impl Default for AuthData {
    fn default() -> Self {
        Self {
            schema_version: default_auth_schema_version(),
            updated_at_unix: now_unix(),
            active_profiles: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }
}

fn default_auth_schema_version() -> u32 {
    AUTH_SCHEMA_VERSION
}

#[derive(Debug, Clone)]
pub(crate) struct AuthService {
    store: AuthStore,
    client: reqwest::Client,
}

impl AuthService {
    pub(crate) fn new_default() -> Result<Self, FrameworkError> {
        Ok(Self {
            store: AuthStore::new(resolve_auth_store_path()?),
            client: reqwest::Client::new(),
        })
    }

    pub(crate) fn default_profile_name() -> &'static str {
        DEFAULT_PROFILE_NAME
    }

    pub(crate) async fn login_openai_codex(
        &self,
        profile_name: &str,
    ) -> Result<(), FrameworkError> {
        let profile_name = normalize_profile_name(profile_name)?;
        let pkce = openai_oauth::generate_pkce_state();
        let authorize_url = openai_oauth::build_authorize_url(&pkce);
        print_browser_login_instructions(&authorize_url);
        let callback_url = read_callback_url_from_terminal()?;
        let code =
            openai_oauth::parse_code_from_callback_url_input(&callback_url, Some(&pkce.state))?;
        let token_set = openai_oauth::exchange_code_for_tokens(&self.client, &code, &pkce).await?;
        let account_id = extract_account_id_from_jwt(&token_set.access_token);
        self.store
            .upsert_oauth_profile(
                OPENAI_CODEX_PROVIDER,
                &profile_name,
                token_set,
                account_id,
                true,
            )
            .await
    }

    pub(crate) async fn logout_openai_codex(
        &self,
        profile_name: Option<&str>,
    ) -> Result<bool, FrameworkError> {
        self.store
            .remove_profile(OPENAI_CODEX_PROVIDER, profile_name)
            .await
    }

    pub(crate) async fn status_openai_codex(
        &self,
    ) -> Result<(Option<String>, Vec<AuthProfile>), FrameworkError> {
        self.store.list_profiles(OPENAI_CODEX_PROVIDER).await
    }

    pub(crate) async fn get_profile(
        &self,
        provider: &str,
        profile_override: Option<&str>,
    ) -> Result<Option<AuthProfile>, FrameworkError> {
        self.store.get_profile(provider, profile_override).await
    }

    pub(crate) async fn get_valid_openai_access_token(
        &self,
        profile_override: Option<&str>,
    ) -> Result<Option<String>, FrameworkError> {
        let Some(mut profile) = self
            .store
            .get_profile(OPENAI_CODEX_PROVIDER, profile_override)
            .await?
        else {
            return Ok(None);
        };

        if !profile
            .token_set
            .is_expiring_within(OPENAI_REFRESH_SKEW_SECS)
        {
            return Ok(Some(profile.token_set.access_token));
        }

        let Some(refresh_token) = profile.token_set.refresh_token.clone() else {
            return Ok(Some(profile.token_set.access_token));
        };

        let mut refreshed =
            openai_oauth::refresh_access_token(&self.client, &refresh_token).await?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token);
        }
        let account_id = extract_account_id_from_jwt(&refreshed.access_token)
            .or_else(|| profile.account_id.clone());
        profile.token_set = refreshed;
        profile.account_id = account_id.clone();
        profile.updated_at_unix = now_unix();

        self.store.update_profile(profile.clone()).await?;
        Ok(Some(profile.token_set.access_token))
    }
}

fn resolve_auth_store_path() -> Result<PathBuf, FrameworkError> {
    resolve_auth_store_path_from(AppPaths::resolve())
}

fn resolve_auth_store_path_from(
    paths: Result<AppPaths, FrameworkError>,
) -> Result<PathBuf, FrameworkError> {
    let paths = paths?;
    Ok(paths.base_dir.join("auth.json"))
}

fn print_browser_login_instructions(authorize_url: &str) {
    println!("Open this URL in your browser to authorize SimpleClaw:");
    println!("{authorize_url}");
    println!("After authorization, copy the full callback URL and paste it below.");
}

fn read_callback_url_from_terminal() -> Result<String, FrameworkError> {
    require_interactive_terminal(io::stdin().is_terminal(), io::stdout().is_terminal())?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    read_callback_url(&mut stdin.lock(), &mut stdout.lock())
}

fn require_interactive_terminal(
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> Result<(), FrameworkError> {
    if stdin_is_terminal && stdout_is_terminal {
        return Ok(());
    }
    Err(FrameworkError::Provider(
        "OpenAI OAuth login requires an interactive terminal to paste the callback URL".to_owned(),
    ))
}

fn read_callback_url<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
) -> Result<String, FrameworkError> {
    write!(output, "Paste callback URL: ").map_err(|err| {
        FrameworkError::Provider(format!(
            "OpenAI OAuth failed to write callback prompt: {err}"
        ))
    })?;
    output.flush().map_err(|err| {
        FrameworkError::Provider(format!(
            "OpenAI OAuth failed to flush callback prompt: {err}"
        ))
    })?;

    let mut callback_url = String::new();
    input.read_line(&mut callback_url).map_err(|err| {
        FrameworkError::Provider(format!("OpenAI OAuth failed to read callback URL: {err}"))
    })?;
    if callback_url.trim().is_empty() {
        return Err(FrameworkError::Provider(
            "OpenAI OAuth callback URL cannot be empty".to_owned(),
        ));
    }
    Ok(callback_url)
}

fn profile_id(provider: &str, profile_name: &str) -> String {
    format!("{provider}:{profile_name}")
}

fn normalize_profile_name(profile_name: &str) -> Result<String, FrameworkError> {
    let trimmed = profile_name.trim();
    if trimmed.is_empty() {
        return Err(FrameworkError::Config(
            "profile name cannot be empty".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

#[derive(Debug, Clone)]
struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    async fn list_profiles(
        &self,
        provider: &str,
    ) -> Result<(Option<String>, Vec<AuthProfile>), FrameworkError> {
        let provider = provider.to_owned();
        self.run_blocking("list_profiles", move |store| {
            store.with_exclusive_data(|data| {
                let mut changed = false;
                if repair_stale_active_profile(data, &provider) {
                    changed = true;
                }
                let active = data.active_profiles.get(&provider).cloned();
                let mut profiles: Vec<AuthProfile> = data
                    .profiles
                    .values()
                    .filter(|profile| profile.provider == provider)
                    .cloned()
                    .collect();
                profiles.sort_by(|left, right| left.profile_name.cmp(&right.profile_name));
                Ok((changed, (active, profiles)))
            })
        })
        .await
    }

    async fn get_profile(
        &self,
        provider: &str,
        profile_override: Option<&str>,
    ) -> Result<Option<AuthProfile>, FrameworkError> {
        let provider = provider.to_owned();
        let profile_override = profile_override.map(str::to_owned);
        self.run_blocking("get_profile", move |store| {
            store.with_exclusive_data(|data| {
                let mut changed = false;
                if repair_stale_active_profile(data, &provider) {
                    changed = true;
                }
                let id = match profile_override.as_deref() {
                    Some(profile_name) => {
                        profile_id(&provider, &normalize_profile_name(profile_name)?)
                    }
                    None => match data.active_profiles.get(&provider) {
                        Some(active) => active.clone(),
                        None => match first_profile_id_for_provider(data, &provider) {
                            Some(first) => first,
                            None => return Ok((changed, None)),
                        },
                    },
                };
                Ok((changed, data.profiles.get(&id).cloned()))
            })
        })
        .await
    }

    async fn upsert_oauth_profile(
        &self,
        provider: &str,
        profile_name: &str,
        token_set: TokenSet,
        account_id: Option<String>,
        set_active: bool,
    ) -> Result<(), FrameworkError> {
        let provider = provider.to_owned();
        let profile_name = profile_name.to_owned();
        self.run_blocking("upsert_oauth_profile", move |store| {
            store.with_lock(|| {
                let now = now_unix();
                let mut data = store.load_unlocked()?;
                let id = profile_id(&provider, &profile_name);
                let created_at_unix = data
                    .profiles
                    .get(&id)
                    .map(|existing| existing.created_at_unix)
                    .unwrap_or(now);
                let profile = AuthProfile {
                    id: id.clone(),
                    provider: provider.clone(),
                    profile_name,
                    account_id,
                    token_set,
                    created_at_unix,
                    updated_at_unix: now,
                };
                data.profiles.insert(id.clone(), profile);
                if set_active {
                    data.active_profiles.insert(provider, id);
                }
                data.updated_at_unix = now;
                store.save_unlocked(&data)?;
                Ok(())
            })
        })
        .await
    }

    async fn remove_profile(
        &self,
        provider: &str,
        profile_name: Option<&str>,
    ) -> Result<bool, FrameworkError> {
        let provider = provider.to_owned();
        let profile_name = profile_name.map(str::to_owned);
        self.run_blocking("remove_profile", move |store| {
            store.with_lock(|| {
                let mut data = store.load_unlocked()?;
                repair_stale_active_profile(&mut data, &provider);
                let profile_id = if let Some(profile_name) = profile_name.as_deref() {
                    profile_id(&provider, &normalize_profile_name(profile_name)?)
                } else {
                    match data.active_profiles.get(&provider) {
                        Some(active) => active.clone(),
                        None => return Ok(false),
                    }
                };

                let removed = data.profiles.remove(&profile_id).is_some();
                if !removed {
                    return Ok(false);
                }

                if data
                    .active_profiles
                    .get(&provider)
                    .map(|active| active == &profile_id)
                    .unwrap_or(false)
                {
                    data.active_profiles.remove(&provider);
                    if let Some(next_active) = data
                        .profiles
                        .values()
                        .filter(|profile| profile.provider == provider)
                        .min_by(|left, right| left.profile_name.cmp(&right.profile_name))
                    {
                        data.active_profiles
                            .insert(provider.clone(), next_active.id.clone());
                    }
                }

                data.updated_at_unix = now_unix();
                store.save_unlocked(&data)?;
                Ok(true)
            })
        })
        .await
    }

    async fn update_profile(&self, profile: AuthProfile) -> Result<(), FrameworkError> {
        self.run_blocking("update_profile", move |store| {
            store.with_lock(|| {
                let mut data = store.load_unlocked()?;
                data.profiles.insert(profile.id.clone(), profile);
                data.updated_at_unix = now_unix();
                store.save_unlocked(&data)
            })
        })
        .await
    }

    async fn run_blocking<T, F>(
        &self,
        operation_name: &'static str,
        f: F,
    ) -> Result<T, FrameworkError>
    where
        T: Send + 'static,
        F: FnOnce(AuthStore) -> Result<T, FrameworkError> + Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || f(store))
            .await
            .map_err(|err| {
                FrameworkError::Io(io::Error::other(format!(
                    "auth blocking task '{operation_name}' failed: {err}"
                )))
            })?
    }

    fn with_exclusive_data<T, F>(&self, mut f: F) -> Result<T, FrameworkError>
    where
        F: FnMut(&mut AuthData) -> Result<(bool, T), FrameworkError>,
    {
        self.with_lock(|| {
            let mut data = self.load_unlocked()?;
            let (changed, value) = f(&mut data)?;
            if changed {
                data.updated_at_unix = now_unix();
                self.save_unlocked(&data)?;
            }
            Ok(value)
        })
    }

    fn with_lock<T, F>(&self, f: F) -> Result<T, FrameworkError>
    where
        F: FnOnce() -> Result<T, FrameworkError>,
    {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_path = lock_path_for_auth_store(&self.path);
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| {
                FrameworkError::Io(std::io::Error::new(
                    err.kind(),
                    format!(
                        "failed to open auth lock file {}: {err}",
                        lock_path.display()
                    ),
                ))
            })?;

        acquire_lock_with_timeout(
            &lock_file,
            &lock_path,
            AUTH_LOCK_TIMEOUT,
            AUTH_LOCK_RETRY_INTERVAL,
        )?;

        let result = f();
        debug!(
            event = "auth.store.lock",
            status = "release_start",
            lock_path = %lock_path.display(),
            "auth store lock"
        );
        let unlock_result = FileExt::unlock(&lock_file).map_err(|err| {
            FrameworkError::Io(std::io::Error::new(
                err.kind(),
                format!("failed to release auth lock {}: {err}", lock_path.display()),
            ))
        });
        if unlock_result.is_ok() {
            debug!(
                event = "auth.store.lock",
                status = "released",
                lock_path = %lock_path.display(),
                "auth store lock"
            );
        } else if let Err(err) = &unlock_result {
            warn!(
                event = "auth.store.lock",
                status = "release_failed",
                lock_path = %lock_path.display(),
                error = %err,
                "auth store lock"
            );
        }

        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    fn load_unlocked(&self) -> Result<AuthData, FrameworkError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthData::default());
            }
            Err(err) => return Err(FrameworkError::Io(err)),
        };
        if content.trim().is_empty() {
            return Ok(AuthData::default());
        }
        serde_json::from_str::<AuthData>(&content).map_err(|err| {
            FrameworkError::Config(format!(
                "failed to parse auth store {}: {err}",
                self.path.display()
            ))
        })
    }

    fn save_unlocked(&self, data: &AuthData) -> Result<(), FrameworkError> {
        let serialized = serde_json::to_string_pretty(data).map_err(|err| {
            FrameworkError::Config(format!("failed to serialize auth store: {err}"))
        })?;
        write_file_atomic(&self.path, &serialized)
    }
}

fn lock_path_for_auth_store(path: &Path) -> PathBuf {
    let mut lock_path = path.to_path_buf();
    if let Some(file_name) = path.file_name().and_then(|value| value.to_str()) {
        lock_path.set_file_name(format!("{file_name}.lock"));
    } else {
        lock_path.set_extension("lock");
    }
    lock_path
}

fn acquire_lock_with_timeout(
    lock_file: &fs::File,
    lock_path: &Path,
    timeout: Duration,
    retry_interval: Duration,
) -> Result<(), FrameworkError> {
    debug!(
        event = "auth.store.lock",
        status = "wait_start",
        lock_path = %lock_path.display(),
        timeout_ms = timeout.as_millis() as u64,
        "auth store lock"
    );
    let started = Instant::now();
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => {
                debug!(
                    event = "auth.store.lock",
                    status = "acquired",
                    lock_path = %lock_path.display(),
                    wait_ms = started.elapsed().as_millis() as u64,
                    "auth store lock"
                );
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    warn!(
                        event = "auth.store.lock",
                        status = "timeout",
                        lock_path = %lock_path.display(),
                        wait_ms = started.elapsed().as_millis() as u64,
                        "auth store lock"
                    );
                    return Err(FrameworkError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out acquiring auth lock {} after {}ms",
                            lock_path.display(),
                            started.elapsed().as_millis()
                        ),
                    )));
                }
                std::thread::sleep(retry_interval);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                warn!(
                    event = "auth.store.lock",
                    status = "acquire_failed",
                    lock_path = %lock_path.display(),
                    error = %err,
                    "auth store lock"
                );
                return Err(FrameworkError::Io(io::Error::new(
                    err.kind(),
                    format!("failed to acquire auth lock {}: {err}", lock_path.display()),
                )));
            }
        }
    }
}

fn first_profile_id_for_provider(data: &AuthData, provider: &str) -> Option<String> {
    data.profiles
        .values()
        .filter(|profile| profile.provider == provider)
        .min_by(|left, right| left.profile_name.cmp(&right.profile_name))
        .map(|profile| profile.id.clone())
}

fn repair_stale_active_profile(data: &mut AuthData, provider: &str) -> bool {
    let Some(active_id) = data.active_profiles.get(provider).cloned() else {
        return false;
    };
    if data.profiles.contains_key(&active_id) {
        return false;
    }

    if let Some(fallback_id) = first_profile_id_for_provider(data, provider) {
        data.active_profiles
            .insert(provider.to_owned(), fallback_id);
    } else {
        data.active_profiles.remove(provider);
    }
    true
}

fn write_file_atomic(path: &PathBuf, content: &str) -> Result<(), FrameworkError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut temp_path = path.clone();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos();
    temp_path.set_extension(format!("tmp-{nonce}"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
    }

    Ok(())
}

pub(crate) fn format_expires_at(expires_at_unix: Option<i64>) -> String {
    let Some(expires_at_unix) = expires_at_unix else {
        return "unknown".to_owned();
    };
    match chrono::DateTime::<chrono::Utc>::from_timestamp(expires_at_unix, 0) {
        Some(datetime) => datetime.to_rfc3339(),
        None => expires_at_unix.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Cursor;

    use tokio::time::{sleep, timeout};

    use super::*;

    fn unique_path(prefix: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_{prefix}_{now}.json"))
    }

    #[test]
    fn read_callback_url_reads_single_line() {
        let mut input = Cursor::new("http://localhost:1455/auth/callback?code=abc&state=xyz\n");
        let mut output = Vec::new();
        let callback_url =
            read_callback_url(&mut input, &mut output).expect("callback URL should be read");
        assert_eq!(
            callback_url.trim(),
            "http://localhost:1455/auth/callback?code=abc&state=xyz"
        );
        assert_eq!(String::from_utf8_lossy(&output), "Paste callback URL: ");
    }

    #[test]
    fn read_callback_url_rejects_empty_input() {
        let mut input = Cursor::new("\n");
        let mut output = Vec::new();
        let err = read_callback_url(&mut input, &mut output).expect_err("empty input should fail");
        assert!(err.to_string().contains("callback URL cannot be empty"));
    }

    #[test]
    fn require_interactive_terminal_rejects_non_terminal_streams() {
        let err = require_interactive_terminal(false, true)
            .expect_err("non-interactive terminal should fail");
        assert!(err.to_string().contains("requires an interactive terminal"));
    }

    #[tokio::test]
    async fn upsert_and_get_profile_round_trips() {
        let path = unique_path("auth");
        let store = AuthStore::new(path.clone());
        let token_set = TokenSet {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            id_token: None,
            expires_at_unix: Some(now_unix() + 3600),
            token_type: Some("Bearer".to_owned()),
            scope: Some("openid".to_owned()),
        };

        store
            .upsert_oauth_profile(
                OPENAI_CODEX_PROVIDER,
                "default",
                token_set,
                Some("acct_123".to_owned()),
                true,
            )
            .await
            .expect("profile should be saved");

        let loaded = store
            .get_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .expect("profile should load")
            .expect("profile should exist");
        assert_eq!(loaded.profile_name, "default");
        assert_eq!(loaded.account_id.as_deref(), Some("acct_123"));

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn remove_profile_clears_active_pointer() {
        let path = unique_path("auth_remove");
        let store = AuthStore::new(path.clone());
        let token_set = TokenSet {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            id_token: None,
            expires_at_unix: Some(now_unix() + 3600),
            token_type: Some("Bearer".to_owned()),
            scope: Some("openid".to_owned()),
        };

        store
            .upsert_oauth_profile(OPENAI_CODEX_PROVIDER, "default", token_set, None, true)
            .await
            .expect("profile should be saved");
        let removed = store
            .remove_profile(OPENAI_CODEX_PROVIDER, Some("default"))
            .await
            .expect("remove should succeed");
        assert!(removed);

        let profile = store
            .get_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .expect("lookup should succeed");
        assert!(profile.is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_accepts_legacy_file_without_schema_version() {
        let path = unique_path("legacy_auth");
        let legacy = r#"{
  "profiles": {
    "openai_codex:default": {
      "id": "openai_codex:default",
      "provider": "openai_codex",
      "profile_name": "default",
      "token_set": {
        "access_token": "abc",
        "refresh_token": "def"
      }
    }
  },
  "active_profiles": {
    "openai_codex": "openai_codex:default"
  }
}"#;
        fs::write(&path, legacy).expect("legacy file should be written");
        let store = AuthStore::new(path.clone());

        let data = store.load_unlocked().expect("legacy auth should parse");
        assert_eq!(data.schema_version, AUTH_SCHEMA_VERSION);
        assert!(data.updated_at_unix > 0);
        assert!(data.profiles.contains_key("openai_codex:default"));

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn get_profile_repairs_stale_active_pointer() {
        let path = unique_path("stale_active");
        let mut data = AuthData::default();
        let fallback_id = profile_id(OPENAI_CODEX_PROVIDER, "backup");
        data.profiles.insert(
            fallback_id.clone(),
            AuthProfile {
                id: fallback_id.clone(),
                provider: OPENAI_CODEX_PROVIDER.to_owned(),
                profile_name: "backup".to_owned(),
                account_id: Some("acct_1".to_owned()),
                token_set: TokenSet {
                    access_token: "access".to_owned(),
                    refresh_token: Some("refresh".to_owned()),
                    id_token: None,
                    expires_at_unix: Some(now_unix() + 3600),
                    token_type: Some("Bearer".to_owned()),
                    scope: None,
                },
                created_at_unix: now_unix(),
                updated_at_unix: now_unix(),
            },
        );
        data.active_profiles.insert(
            OPENAI_CODEX_PROVIDER.to_owned(),
            "openai_codex:missing".to_owned(),
        );
        fs::write(
            &path,
            serde_json::to_string_pretty(&data).expect("auth fixture should serialize"),
        )
        .expect("fixture should write");

        let store = AuthStore::new(path.clone());
        let profile = store
            .get_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .expect("profile lookup should succeed")
            .expect("fallback profile should be returned");
        assert_eq!(profile.id, fallback_id);

        let repaired = store.load_unlocked().expect("store should parse");
        assert_eq!(
            repaired
                .active_profiles
                .get(OPENAI_CODEX_PROVIDER)
                .map(String::as_str),
            Some(fallback_id.as_str())
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn remove_profile_without_name_handles_stale_active_pointer() {
        let path = unique_path("remove_stale_active");
        let store = AuthStore::new(path.clone());
        let token_set = TokenSet {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            id_token: None,
            expires_at_unix: Some(now_unix() + 3600),
            token_type: Some("Bearer".to_owned()),
            scope: Some("openid".to_owned()),
        };
        store
            .upsert_oauth_profile(OPENAI_CODEX_PROVIDER, "backup", token_set, None, false)
            .await
            .expect("profile should save");

        let mut data = store.load_unlocked().expect("store should load");
        data.active_profiles.insert(
            OPENAI_CODEX_PROVIDER.to_owned(),
            "openai_codex:missing".to_owned(),
        );
        store.save_unlocked(&data).expect("store should save");

        let removed = store
            .remove_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .expect("remove should succeed");
        assert!(removed);
        let remaining = store
            .get_profile(OPENAI_CODEX_PROVIDER, None)
            .await
            .expect("lookup should succeed");
        assert!(remaining.is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn resolve_auth_store_path_propagates_path_errors() {
        let err = resolve_auth_store_path_from(Err(FrameworkError::Config("boom".to_owned())))
            .expect_err("error should propagate");
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn acquire_lock_with_timeout_returns_timed_out_when_lock_is_held() {
        let path = unique_path("lock_timeout");
        let lock_path = lock_path_for_auth_store(&path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).expect("lock directory should exist");
        }
        let held = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("held lock file should open");
        held.lock_exclusive()
            .expect("test should acquire initial lock");

        let contender = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("contender lock file should open");
        let err = acquire_lock_with_timeout(
            &contender,
            &lock_path,
            Duration::from_millis(60),
            Duration::from_millis(10),
        )
        .expect_err("contender should time out");
        assert!(err.to_string().contains("timed out acquiring auth lock"));

        FileExt::unlock(&held).expect("held lock should unlock");
        let _ = fs::remove_file(&lock_path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_profile_waits_on_lock_without_blocking_runtime() {
        let path = unique_path("lock_runtime");
        let store = AuthStore::new(path.clone());
        let lock_path = lock_path_for_auth_store(&path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).expect("lock directory should exist");
        }
        let held = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("held lock file should open");
        held.lock_exclusive()
            .expect("test should acquire initial lock");

        let lookup = tokio::spawn({
            let store = store.clone();
            async move { store.get_profile(OPENAI_CODEX_PROVIDER, None).await }
        });

        timeout(Duration::from_millis(200), async {
            sleep(Duration::from_millis(30)).await;
        })
        .await
        .expect("runtime should make forward progress while lock is held");

        FileExt::unlock(&held).expect("held lock should unlock");
        let result = timeout(Duration::from_secs(1), lookup)
            .await
            .expect("lookup should complete after lock release")
            .expect("task should not panic")
            .expect("lookup should succeed");
        assert!(result.is_none());

        let _ = fs::remove_file(path);
        let _ = fs::remove_file(lock_path);
    }
}
