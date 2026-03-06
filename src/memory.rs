use std::cmp::Ordering;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chrono::Duration;
use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use fastembed::{InitOptions, TextEmbedding};
use rusqlite::params;
use tokio::sync::{Mutex, mpsc};

use crate::config::{DatabaseConfig, EmbeddingConfig};
use crate::error::FrameworkError;
use crate::paths::AppPaths;

#[derive(Clone)]
pub struct MemoryStore {
    pool: Pool,
    long_term_pool: Pool,
    embedder: Arc<Mutex<TextEmbedding>>,
    ingest_tx: mpsc::Sender<IngestItem>,
}

const MEMORIZE_DEDUPE_WINDOW_SECS: i64 = 300;

#[derive(Debug)]
struct IngestItem {
    session_id: String,
    content: String,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub username: Option<String>,
}

impl MemoryStore {
    pub async fn new(
        short_term_path: &Path,
        long_term_path: &Path,
        db_config: &DatabaseConfig,
        _embedding_config: &EmbeddingConfig,
    ) -> Result<Self, FrameworkError> {
        register_sqlite_vec()?;

        let cfg = PoolConfig::new(short_term_path);
        let pool = cfg
            .create_pool(Runtime::Tokio1)
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let long_term_cfg = PoolConfig::new(long_term_path);
        let long_term_pool = long_term_cfg
            .create_pool(Runtime::Tokio1)
            .map_err(|e| FrameworkError::Config(e.to_string()))?;

        let paths = AppPaths::resolve()?;
        paths.ensure_fastembed_cache_dir()?;
        let mut embedder_options = InitOptions::default();
        embedder_options.cache_dir = paths.fastembed_cache_dir.clone();
        let embedder = TextEmbedding::try_new(embedder_options)
            .map_err(|e| FrameworkError::Config(format!("failed to initialize embedder: {e}")))?;

        let (ingest_tx, ingest_rx) = mpsc::channel(512);
        let this = Self {
            pool,
            long_term_pool,
            embedder: Arc::new(Mutex::new(embedder)),
            ingest_tx,
        };

        this.init_schema(db_config).await?;
        this.init_long_term_schema(db_config).await?;
        this.spawn_ingest_worker(ingest_rx);
        Ok(this)
    }

    async fn init_schema(&self, db_config: &DatabaseConfig) -> Result<(), FrameworkError> {
        let busy_timeout = db_config.busy_timeout_ms as i64;
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        conn.interact(move |conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "busy_timeout", busy_timeout)?;
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    username TEXT,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS vec_memory (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    embedding BLOB,
                    created_at TEXT NOT NULL
                );
                "#,
            )?;

            let _: String = conn.query_row("SELECT vec_version()", [], |row| row.get(0))?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;

        Ok(())
    }

    async fn init_long_term_schema(
        &self,
        db_config: &DatabaseConfig,
    ) -> Result<(), FrameworkError> {
        let busy_timeout = db_config.busy_timeout_ms as i64;
        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        conn.interact(move |conn| {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "busy_timeout", busy_timeout)?;
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS ltm_facts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    source_session_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    importance INTEGER NOT NULL,
                    embedding BLOB,
                    created_at TEXT NOT NULL
                );
                "#,
            )?;

            let _: String = conn.query_row("SELECT vec_version()", [], |row| row.get(0))?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;

        Ok(())
    }

    fn spawn_ingest_worker(&self, mut ingest_rx: mpsc::Receiver<IngestItem>) {
        let pool = self.pool.clone();
        let embedder = Arc::clone(&self.embedder);
        tokio::spawn(async move {
            while let Some(item) = ingest_rx.recv().await {
                let embedding = match embed_text(&embedder, &item.content).await {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::error!(error = %err, "embedding generation failed");
                        continue;
                    }
                };

                let now = chrono::Utc::now().to_rfc3339();
                let blob = encode_f32_blob(&embedding);
                let session = item.session_id;
                let content = item.content;

                let conn = match pool.get().await {
                    Ok(c) => c,
                    Err(err) => {
                        tracing::error!(error = %err, "failed to acquire db connection for ingest");
                        continue;
                    }
                };

                if let Err(err) = conn
                    .interact(move |conn| {
                        conn.execute(
                            "INSERT INTO vec_memory (session_id, content, embedding, created_at) VALUES (?1, ?2, ?3, ?4)",
                            params![session, content, blob, now],
                        )?;
                        Ok::<(), rusqlite::Error>(())
                    })
                    .await
                {
                    tracing::error!(error = %err, "vec_memory ingest task failed");
                }
            }
        });
    }

    pub async fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        username: Option<&str>,
    ) -> Result<(), FrameworkError> {
        let now = chrono::Utc::now().to_rfc3339();
        let session_for_sessions = session_id.to_owned();
        let session_for_messages = session_id.to_owned();
        let role = role.to_owned();
        let content_owned = content.to_owned();
        let username_owned = username
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let now_for_session = now.clone();
        let now_for_message = now;

        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        conn.interact(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO sessions (id, created_at) VALUES (?1, ?2)",
                params![session_for_sessions, now_for_session],
            )?;
            conn.execute(
                "INSERT INTO messages (session_id, role, content, username, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_for_messages,
                    role,
                    content_owned,
                    username_owned,
                    now_for_message
                ],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;

        self.ingest_tx
            .send(IngestItem {
                session_id: session_id.to_owned(),
                content: content.to_owned(),
            })
            .await
            .map_err(|e| FrameworkError::Config(format!("failed to queue memory ingest: {e}")))?;

        Ok(())
    }

    pub async fn semantic_query_combined(
        &self,
        session_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<String>, FrameworkError> {
        let query_embedding = embed_text(&self.embedder, query).await?;
        let session = session_id.to_owned();

        let short_term_conn = self
            .pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let short_term_rows = short_term_conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT content, embedding FROM vec_memory WHERE session_id = ?1 AND embedding IS NOT NULL",
                )?;
                let mut out = Vec::new();
                let mapped = stmt.query_map(params![session], |row| {
                    let content: String = row.get(0)?;
                    let blob: Vec<u8> = row.get(1)?;
                    Ok::<(String, Vec<u8>), rusqlite::Error>((content, blob))
                })?;
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<(String, Vec<u8>)>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

        let long_term_conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let long_term_rows = long_term_conn
            .interact(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT content, embedding, kind, importance FROM ltm_facts WHERE embedding IS NOT NULL",
                )?;
                let mut out = Vec::new();
                let mapped = stmt.query_map([], |row| {
                    let content: String = row.get(0)?;
                    let blob: Vec<u8> = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let importance: i64 = row.get(3)?;
                    Ok::<(String, Vec<u8>, String, i64), rusqlite::Error>((
                        content, blob, kind, importance,
                    ))
                })?;
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<(String, Vec<u8>, String, i64)>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

        let mut scored: Vec<(f32, String)> = Vec::new();
        for (content, blob) in short_term_rows {
            let emb = decode_f32_blob(&blob);
            if emb.is_empty() {
                continue;
            }
            let score = cosine_similarity(&query_embedding, &emb);
            scored.push((score, format!("[short-term] {content}")));
        }

        for (content, blob, kind, importance) in long_term_rows {
            let emb = decode_f32_blob(&blob);
            if emb.is_empty() {
                continue;
            }
            let score = cosine_similarity(&query_embedding, &emb);
            let weighted = score + ((importance as f32).clamp(1.0, 5.0) - 1.0) * 0.02;
            scored.push((weighted, format!("[long-term/{kind}] {content}")));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(_, text)| text)
            .collect())
    }

    pub async fn recent_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, FrameworkError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let session = session_id.to_owned();
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let mut rows = conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT role, content, username FROM messages
                     WHERE session_id = ?1 AND (role = 'user' OR role = 'assistant')
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let mapped = stmt.query_map(params![session, limit as i64], |row| {
                    Ok::<StoredMessage, rusqlite::Error>(StoredMessage {
                        role: row.get(0)?,
                        content: row.get(1)?,
                        username: row.get(2)?,
                    })
                })?;

                let mut out = Vec::new();
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<StoredMessage>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;
        rows.reverse();
        Ok(rows)
    }

    pub async fn memorize(
        &self,
        session_id: &str,
        content: &str,
        kind: &str,
        importance: u8,
    ) -> Result<bool, FrameworkError> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(FrameworkError::Tool(
                "memorize requires non-empty content".to_owned(),
            ));
        }

        let importance = importance.clamp(1, 5) as i64;
        let now_dt = chrono::Utc::now();
        let dedupe_cutoff = (now_dt - Duration::seconds(MEMORIZE_DEDUPE_WINDOW_SECS)).to_rfc3339();
        let now = now_dt.to_rfc3339();
        let session = session_id.to_owned();
        let fact = trimmed.to_owned();
        let kind = {
            let trimmed_kind = kind.trim();
            if trimmed_kind.is_empty() {
                "general".to_owned()
            } else {
                trimmed_kind.to_owned()
            }
        };
        let session_for_check = session.clone();
        let fact_for_check = fact.clone();
        let kind_for_check = kind.clone();

        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let already_exists = conn
            .interact(move |conn| {
                has_recent_long_term_fact(
                    conn,
                    &session_for_check,
                    &kind_for_check,
                    &fact_for_check,
                    &dedupe_cutoff,
                )
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;
        if already_exists {
            return Ok(false);
        }

        let embedding = embed_text(&self.embedder, trimmed).await?;
        let blob = encode_f32_blob(&embedding);

        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO ltm_facts (source_session_id, content, kind, importance, embedding, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session, fact, kind, importance, blob, now],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;

        Ok(true)
    }
}

fn has_recent_long_term_fact(
    conn: &rusqlite::Connection,
    session_id: &str,
    kind: &str,
    content: &str,
    dedupe_cutoff: &str,
) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT 1
         FROM ltm_facts
         WHERE source_session_id = ?1
           AND kind = ?2
           AND content = ?3
           AND created_at >= ?4
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![session_id, kind, content, dedupe_cutoff])?;
    Ok(rows.next()?.is_some())
}

fn register_sqlite_vec() -> Result<(), FrameworkError> {
    static RESULT: OnceLock<Result<(), i32>> = OnceLock::new();

    let result = RESULT.get_or_init(|| unsafe {
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
        if rc == rusqlite::ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(rc)
        }
    });

    result.map_err(|rc| {
        FrameworkError::Config(format!(
            "failed to register bundled sqlite-vec extension (sqlite3_auto_extension rc={rc})"
        ))
    })
}

async fn embed_text(
    embedder: &Arc<Mutex<TextEmbedding>>,
    text: &str,
) -> Result<Vec<f32>, FrameworkError> {
    let mut model = embedder.lock().await;
    let embeddings = model
        .embed(vec![text.to_owned()], None)
        .map_err(|e| FrameworkError::Config(format!("embedding failed: {e}")))?;
    embeddings
        .into_iter()
        .next()
        .ok_or_else(|| FrameworkError::Config("embedder returned no vector".to_owned()))
}

fn encode_f32_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn decode_f32_blob(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() % 4 != 0 {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }

    let mut dot = 0.0_f32;
    let mut a_norm = 0.0_f32;
    let mut b_norm = 0.0_f32;
    for i in 0..len {
        dot += a[i] * b[i];
        a_norm += a[i] * a[i];
        b_norm += b[i] * b[i];
    }

    let denom = a_norm.sqrt() * b_norm.sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::has_recent_long_term_fact;

    #[test]
    fn long_term_duplicate_check_matches_recent_identical_fact() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite should open");
        conn.execute_batch(
            r#"
            CREATE TABLE ltm_facts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                kind TEXT NOT NULL,
                importance INTEGER NOT NULL,
                embedding BLOB,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .expect("ltm_facts table should be created");

        conn.execute(
            "INSERT INTO ltm_facts (source_session_id, content, kind, importance, embedding, created_at) VALUES (?1, ?2, ?3, 3, NULL, ?4)",
            rusqlite::params![
                "chan:design",
                "The squire loves bananas.",
                "general",
                "2026-03-06T19:11:44.000000+00:00"
            ],
        )
        .expect("insert should succeed");

        let matched = has_recent_long_term_fact(
            &conn,
            "chan:design",
            "general",
            "The squire loves bananas.",
            "2026-03-06T19:11:40.000000+00:00",
        )
        .expect("query should succeed");
        assert!(matched);

        let too_old = has_recent_long_term_fact(
            &conn,
            "chan:design",
            "general",
            "The squire loves bananas.",
            "2026-03-06T19:11:45.000000+00:00",
        )
        .expect("query should succeed");
        assert!(!too_old);
    }
}
