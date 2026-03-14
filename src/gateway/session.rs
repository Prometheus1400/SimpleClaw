use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::GatewayChannelKind;
use crate::error::FrameworkError;

pub(super) fn build_session_scope_key(
    agent_id: &str,
    is_dm: bool,
    source: GatewayChannelKind,
    channel_id: &str,
) -> String {
    if is_dm {
        format!("agent:{agent_id}:main")
    } else {
        match source {
            GatewayChannelKind::Discord => format!("agent:{agent_id}:discord:{channel_id}"),
        }
    }
}

fn format_session_id(scope_key: &str, sequence: i64) -> String {
    format!("{scope_key}:session:{sequence}")
}

#[derive(Clone, Debug)]
pub(crate) struct SessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl SessionStore {
    pub(crate) fn open(path: &Path) -> Result<Self, FrameworkError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        Self::open_with_connection(conn)
    }

    pub(crate) fn in_memory() -> Result<Self, FrameworkError> {
        let conn = Connection::open_in_memory()?;
        Self::open_with_connection(conn)
    }

    fn open_with_connection(conn: Connection) -> Result<Self, FrameworkError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000_u64)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS active_sessions (
                scope_key TEXT PRIMARY KEY,
                active_session_id TEXT NOT NULL,
                active_sequence INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) async fn current_or_create(
        &self,
        scope_key: &str,
    ) -> Result<String, FrameworkError> {
        let conn = self.conn.lock().await;
        if let Some((session_id, _sequence)) = select_active_session(&conn, scope_key)? {
            return Ok(session_id);
        }

        let sequence = 1_i64;
        let session_id = format_session_id(scope_key, sequence);
        conn.execute(
            "
            INSERT INTO active_sessions (scope_key, active_session_id, active_sequence, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ",
            params![scope_key, session_id, sequence, Utc::now().to_rfc3339()],
        )?;
        Ok(format_session_id(scope_key, sequence))
    }

    pub(crate) async fn rotate(&self, scope_key: &str) -> Result<SessionRotation, FrameworkError> {
        let conn = self.conn.lock().await;
        let (previous_session_id, previous_sequence) =
            match select_active_session(&conn, scope_key)? {
                Some((session_id, sequence)) => (session_id, sequence),
                None => {
                    let sequence = 1_i64;
                    (format_session_id(scope_key, sequence), sequence)
                }
            };
        let next_sequence = previous_sequence + 1;
        let session_id = format_session_id(scope_key, next_sequence);
        conn.execute(
            "
            INSERT INTO active_sessions (scope_key, active_session_id, active_sequence, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(scope_key) DO UPDATE SET
                active_session_id = excluded.active_session_id,
                active_sequence = excluded.active_sequence,
                updated_at = excluded.updated_at
            ",
            params![
                scope_key,
                session_id,
                next_sequence,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(SessionRotation {
            previous_session_id,
            session_id,
        })
    }
}

fn select_active_session(
    conn: &Connection,
    scope_key: &str,
) -> Result<Option<(String, i64)>, FrameworkError> {
    conn.query_row(
        "
        SELECT active_session_id, active_sequence
        FROM active_sessions
        WHERE scope_key = ?1
        ",
        params![scope_key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(FrameworkError::from)
}

pub(crate) struct SessionRotation {
    pub previous_session_id: String,
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{SessionStore, build_session_scope_key};
    use crate::config::GatewayChannelKind;

    #[test]
    fn dm_session_scope_key_uses_main_suffix() {
        assert_eq!(
            build_session_scope_key("default", true, GatewayChannelKind::Discord, "123"),
            "agent:default:main"
        );
    }

    #[test]
    fn non_dm_session_scope_key_uses_discord_channel() {
        assert_eq!(
            build_session_scope_key("default", false, GatewayChannelKind::Discord, "123"),
            "agent:default:discord:123"
        );
    }

    #[tokio::test]
    async fn store_creates_scoped_session_id_on_first_lookup() {
        let path = unique_db_path("create");
        let store = SessionStore::open(&path).expect("store should open");

        assert_eq!(
            store
                .current_or_create("agent:default:discord:123")
                .await
                .expect("session lookup should succeed"),
            "agent:default:discord:123:session:1"
        );
    }

    #[tokio::test]
    async fn store_persists_rotated_session_across_reopen() {
        let path = unique_db_path("persist");
        let store = SessionStore::open(&path).expect("store should open");
        let rotated = store
            .rotate("agent:default:discord:123")
            .await
            .expect("rotation should succeed");
        assert_eq!(
            rotated.previous_session_id,
            "agent:default:discord:123:session:1"
        );
        assert_eq!(rotated.session_id, "agent:default:discord:123:session:2");

        let reopened = SessionStore::open(&path).expect("reopened store should open");
        assert_eq!(
            reopened
                .current_or_create("agent:default:discord:123")
                .await
                .expect("reopened lookup should succeed"),
            "agent:default:discord:123:session:2"
        );
    }

    fn unique_db_path(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_session_store_{prefix}_{nanos}.db"))
    }
}
