use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::FrameworkError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CronJob {
    pub id: String,
    pub agent_id: String,
    pub schedule: String,
    pub prompt: String,
    pub guard_command: Option<String>,
    pub workspace_root: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub source_channel: String,
    pub is_dm: bool,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub guard_timeout_seconds: u64,
    pub enabled: bool,
}

#[derive(Debug)]
pub(crate) struct CronStore {
    conn: Connection,
}

impl CronStore {
    pub(crate) fn open(path: &Path) -> Result<Self, FrameworkError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000_u64)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS cron_jobs (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                schedule TEXT NOT NULL,
                prompt TEXT NOT NULL,
                guard_command TEXT,
                workspace_root TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                guild_id TEXT,
                source_channel TEXT NOT NULL,
                is_dm INTEGER NOT NULL DEFAULT 0,
                created_by TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_fired_at TEXT,
                guard_timeout_seconds INTEGER NOT NULL DEFAULT 10,
                enabled INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_cron_jobs_agent ON cron_jobs(agent_id);
            ",
        )?;

        Ok(Self { conn })
    }

    pub(crate) fn create_job(&self, job: &CronJob) -> Result<(), FrameworkError> {
        self.conn.execute(
            "
            INSERT INTO cron_jobs (
                id, agent_id, schedule, prompt, guard_command, workspace_root,
                channel_id, guild_id, source_channel, is_dm,
                created_by, created_at, last_fired_at, guard_timeout_seconds, enabled
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15
            )
            ",
            params![
                job.id,
                job.agent_id,
                job.schedule,
                job.prompt,
                job.guard_command,
                job.workspace_root,
                job.channel_id,
                job.guild_id,
                job.source_channel,
                bool_to_i64(job.is_dm),
                job.created_by,
                job.created_at.to_rfc3339(),
                job.last_fired_at.as_ref().map(DateTime::<Utc>::to_rfc3339),
                job.guard_timeout_seconds as i64,
                bool_to_i64(job.enabled),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn delete_job(&self, id: &str, agent_id: &str) -> Result<bool, FrameworkError> {
        let affected = self.conn.execute(
            "DELETE FROM cron_jobs WHERE id = ?1 AND agent_id = ?2",
            params![id, agent_id],
        )?;
        Ok(affected > 0)
    }

    pub(crate) fn list_jobs(
        &self,
        agent_id: &str,
        query: Option<&str>,
    ) -> Result<Vec<CronJob>, FrameworkError> {
        let mut out = Vec::new();
        if let Some(query) = query {
            let mut stmt = self.conn.prepare(
                "
                SELECT
                    id, agent_id, schedule, prompt, guard_command, workspace_root,
                    channel_id, guild_id, source_channel, is_dm,
                    created_by, created_at, last_fired_at, guard_timeout_seconds, enabled
                FROM cron_jobs
                WHERE agent_id = ?1
                  AND (
                    lower(id) LIKE ?2 OR
                    lower(schedule) LIKE ?2 OR
                    lower(prompt) LIKE ?2
                  )
                ORDER BY created_at DESC
                ",
            )?;
            let query_like = format!("%{}%", query.to_ascii_lowercase());
            let rows = stmt.query_map(params![agent_id, query_like], parse_row)?;
            for row in rows {
                out.push(row?);
            }
            return Ok(out);
        }

        let mut stmt = self.conn.prepare(
            "
            SELECT
                id, agent_id, schedule, prompt, guard_command, workspace_root,
                channel_id, guild_id, source_channel, is_dm,
                created_by, created_at, last_fired_at, guard_timeout_seconds, enabled
            FROM cron_jobs
            WHERE agent_id = ?1
            ORDER BY created_at DESC
            ",
        )?;
        let rows = stmt.query_map(params![agent_id], parse_row)?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub(crate) fn list_all_enabled(&self) -> Result<Vec<CronJob>, FrameworkError> {
        let mut stmt = self.conn.prepare(
            "
            SELECT
                id, agent_id, schedule, prompt, guard_command, workspace_root,
                channel_id, guild_id, source_channel, is_dm,
                created_by, created_at, last_fired_at, guard_timeout_seconds, enabled
            FROM cron_jobs
            WHERE enabled = 1
            ",
        )?;
        let rows = stmt.query_map([], parse_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub(crate) fn update_last_fired(
        &self,
        id: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<(), FrameworkError> {
        self.conn.execute(
            "UPDATE cron_jobs SET last_fired_at = ?2 WHERE id = ?1",
            params![id, timestamp.to_rfc3339()],
        )?;
        Ok(())
    }

    pub(crate) fn count_jobs_for_agent(&self, agent_id: &str) -> Result<u32, FrameworkError> {
        let count: Option<u32> = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cron_jobs WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(count.unwrap_or(0))
    }
}

fn parse_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let created_at_raw: String = row.get(11)?;
    let last_fired_raw: Option<String> = row.get(12)?;

    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })?;

    let last_fired_at = match last_fired_raw {
        Some(raw) => Some(
            chrono::DateTime::parse_from_rfc3339(&raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        12,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
        ),
        None => None,
    };

    Ok(CronJob {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        schedule: row.get(2)?,
        prompt: row.get(3)?,
        guard_command: row.get(4)?,
        workspace_root: row.get(5)?,
        channel_id: row.get(6)?,
        guild_id: row.get(7)?,
        source_channel: row.get(8)?,
        is_dm: row.get::<_, i64>(9)? != 0,
        created_by: row.get(10)?,
        created_at,
        last_fired_at,
        guard_timeout_seconds: row.get::<_, i64>(13)? as u64,
        enabled: row.get::<_, i64>(14)? != 0,
    })
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Utc;

    use super::{CronJob, CronStore};

    fn unique_db_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("simpleclaw_cron_store_{nanos}.db"))
    }

    fn sample_job(id: &str) -> CronJob {
        CronJob {
            id: id.to_owned(),
            agent_id: "agent-a".to_owned(),
            schedule: "*/5 * * * *".to_owned(),
            prompt: "check status".to_owned(),
            guard_command: Some("echo ok".to_owned()),
            workspace_root: "/tmp".to_owned(),
            channel_id: "channel-1".to_owned(),
            guild_id: Some("guild-1".to_owned()),
            source_channel: "discord".to_owned(),
            is_dm: false,
            created_by: "owner".to_owned(),
            created_at: Utc::now(),
            last_fired_at: None,
            guard_timeout_seconds: 10,
            enabled: true,
        }
    }

    #[test]
    fn open_create_list_delete_and_count() {
        let path = unique_db_path();
        let store = CronStore::open(&path).expect("store should open");

        let first = sample_job("job-1");
        let second = sample_job("job-2");
        store.create_job(&first).expect("first insert should work");
        store
            .create_job(&second)
            .expect("second insert should work");

        let listed = store
            .list_jobs("agent-a", None)
            .expect("list should work for agent");
        assert_eq!(listed.len(), 2);

        let filtered = store
            .list_jobs("agent-a", Some("job-1"))
            .expect("query list should work");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "job-1");

        let count = store
            .count_jobs_for_agent("agent-a")
            .expect("count should work");
        assert_eq!(count, 2);

        let deleted = store
            .delete_job("job-2", "agent-a")
            .expect("delete should work");
        assert!(deleted);

        let count_after = store
            .count_jobs_for_agent("agent-a")
            .expect("count after delete should work");
        assert_eq!(count_after, 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_last_fired_persists_timestamp() {
        let path = unique_db_path();
        let store = CronStore::open(&path).expect("store should open");

        let job = sample_job("job-1");
        store.create_job(&job).expect("insert should work");

        let now = Utc::now();
        store
            .update_last_fired("job-1", now)
            .expect("update should work");

        let listed = store
            .list_jobs("agent-a", Some("job-1"))
            .expect("list should work");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last_fired_at, Some(now));

        let _ = std::fs::remove_file(path);
    }
}
