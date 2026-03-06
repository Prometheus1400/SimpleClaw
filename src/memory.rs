use std::cmp::Ordering;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chrono::Duration;
use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use fastembed::{InitOptions, TextEmbedding};
use rusqlite::params;
use tokio::sync::{Mutex, mpsc};
use tracing::trace;

use crate::config::{DatabaseConfig, EmbeddingConfig, MemoryPreinjectConfig};
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

#[derive(Debug, Clone)]
pub struct LongTermForgetMatch {
    pub id: i64,
    pub content: String,
    pub kind: String,
    pub importance: i64,
    pub similarity: f32,
}

#[derive(Debug, Clone)]
pub struct LongTermForgetResult {
    pub matches: Vec<LongTermForgetMatch>,
    pub deleted_count: usize,
    pub similarity_threshold: f32,
    pub max_matches: usize,
    pub kind_filter: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryHitStore {
    LongTerm,
}

#[derive(Debug, Clone)]
pub struct MemoryPreinjectHit {
    pub store: MemoryHitStore,
    pub content: String,
    pub kind: Option<String>,
    pub importance: Option<i64>,
    pub raw_similarity: f32,
    pub final_score: f32,
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
        let top_k = top_k.max(1);
        let top_k_u32 = match u32::try_from(top_k) {
            Ok(value) => value,
            Err(_) => u32::MAX,
        };
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: top_k_u32,
            min_score: 0.0,
            max_items_per_store: 20,
            long_term_weight: 1.0,
            max_chars: 4000,
        };
        let hits = self
            .query_preinject_hits(session_id, query, &config)
            .await?;
        Ok(hits
            .into_iter()
            .map(|hit| match hit.store {
                MemoryHitStore::LongTerm => format!(
                    "[long-term/{}] {}",
                    hit.kind.as_deref().unwrap_or("general"),
                    hit.content
                ),
            })
            .collect())
    }

    pub async fn query_preinject_hits(
        &self,
        session_id: &str,
        query: &str,
        config: &MemoryPreinjectConfig,
    ) -> Result<Vec<MemoryPreinjectHit>, FrameworkError> {
        let config = config.normalized();
        let query_embedding = embed_text(&self.embedder, query).await?;
        let _ = session_id;

        let long_limit = config.max_items_per_store as i64;
        let long_term_conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let long_term_rows = long_term_conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT content, embedding, kind, importance
                     FROM ltm_facts
                     WHERE embedding IS NOT NULL
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let mapped = stmt.query_map(params![long_limit], |row| {
                    let content: String = row.get(0)?;
                    let blob: Vec<u8> = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let importance: i64 = row.get(3)?;
                    Ok::<(String, Vec<u8>, String, i64), rusqlite::Error>((
                        content, blob, kind, importance,
                    ))
                })?;
                let mut out = Vec::new();
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<(String, Vec<u8>, String, i64)>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

        let mut candidates = Vec::new();
        for (content, blob, kind, importance) in long_term_rows {
            let emb = decode_f32_blob(&blob);
            if emb.is_empty() {
                continue;
            }
            let similarity = cosine_similarity(&query_embedding, &emb);
            let with_importance = similarity + ((importance as f32).clamp(1.0, 5.0) - 1.0) * 0.02;
            candidates.push(MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content,
                kind: Some(kind),
                importance: Some(importance),
                raw_similarity: similarity,
                final_score: with_importance * config.long_term_weight,
            });
        }

        Ok(rank_preinject_hits(candidates, &config))
    }

    pub async fn semantic_forget_long_term(
        &self,
        query: &str,
        similarity_threshold: f32,
        max_matches: usize,
        kind_filter: Option<&str>,
        commit: bool,
    ) -> Result<LongTermForgetResult, FrameworkError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(FrameworkError::Tool(
                "forget requires non-empty query".to_owned(),
            ));
        }

        let threshold = similarity_threshold.clamp(0.0, 1.0);
        let max_matches = max_matches.clamp(1, 50);
        let query_embedding = embed_text(&self.embedder, trimmed).await?;
        let normalized_kind = kind_filter
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let rows = if let Some(kind) = normalized_kind.clone() {
            conn.interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, content, embedding, kind, importance
                     FROM ltm_facts
                     WHERE embedding IS NOT NULL AND kind = ?1",
                )?;
                let mut out = Vec::new();
                let mapped = stmt.query_map(params![kind], |row| {
                    let id: i64 = row.get(0)?;
                    let content: String = row.get(1)?;
                    let blob: Vec<u8> = row.get(2)?;
                    let kind: String = row.get(3)?;
                    let importance: i64 = row.get(4)?;
                    Ok::<LongTermEmbeddedRow, rusqlite::Error>(LongTermEmbeddedRow {
                        id,
                        content,
                        embedding: blob,
                        kind,
                        importance,
                    })
                })?;
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<LongTermEmbeddedRow>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??
        } else {
            conn.interact(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, content, embedding, kind, importance
                     FROM ltm_facts
                     WHERE embedding IS NOT NULL",
                )?;
                let mut out = Vec::new();
                let mapped = stmt.query_map([], |row| {
                    let id: i64 = row.get(0)?;
                    let content: String = row.get(1)?;
                    let blob: Vec<u8> = row.get(2)?;
                    let kind: String = row.get(3)?;
                    let importance: i64 = row.get(4)?;
                    Ok::<LongTermEmbeddedRow, rusqlite::Error>(LongTermEmbeddedRow {
                        id,
                        content,
                        embedding: blob,
                        kind,
                        importance,
                    })
                })?;
                for row in mapped {
                    out.push(row?);
                }
                Ok::<Vec<LongTermEmbeddedRow>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??
        };

        let matches = select_forget_matches(rows, &query_embedding, threshold, max_matches);
        let mut deleted_count = 0usize;
        if commit && !matches.is_empty() {
            let ids = matches.iter().map(|item| item.id).collect::<Vec<_>>();
            deleted_count = conn
                .interact(move |conn| {
                    let tx = conn.transaction()?;
                    let mut count = 0usize;
                    for id in ids {
                        count += tx.execute("DELETE FROM ltm_facts WHERE id = ?1", params![id])?;
                    }
                    tx.commit()?;
                    Ok::<usize, rusqlite::Error>(count)
                })
                .await
                .map_err(|e| FrameworkError::Config(e.to_string()))??;
        }

        Ok(LongTermForgetResult {
            matches,
            deleted_count,
            similarity_threshold: threshold,
            max_matches,
            kind_filter: normalized_kind,
        })
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
        let fact_for_check = fact.clone();
        let kind_for_check = kind.clone();

        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let already_exists = conn
            .interact(move |conn| {
                has_recent_long_term_fact(conn, &kind_for_check, &fact_for_check, &dedupe_cutoff)
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
    kind: &str,
    content: &str,
    dedupe_cutoff: &str,
) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT 1
         FROM ltm_facts
         WHERE kind = ?1
           AND content = ?2
           AND created_at >= ?3
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![kind, content, dedupe_cutoff])?;
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

#[derive(Debug)]
struct LongTermEmbeddedRow {
    id: i64,
    content: String,
    embedding: Vec<u8>,
    kind: String,
    importance: i64,
}

fn rank_preinject_hits(
    mut candidates: Vec<MemoryPreinjectHit>,
    config: &MemoryPreinjectConfig,
) -> Vec<MemoryPreinjectHit> {
    let normalized = config.normalized();
    candidates.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(Ordering::Equal)
    });

    let mut dedupe = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in candidates {
        if item.raw_similarity < normalized.min_score {
            trace!(
                store = %memory_store_name(item.store),
                final_score = item.final_score,
                raw_similarity = item.raw_similarity,
                min_score = normalized.min_score,
                "long-term memory pre-injection candidate filtered by raw_similarity min_score"
            );
            continue;
        }
        let key = normalize_memory_key(&item.content);
        if key.is_empty() {
            trace!(
                store = %memory_store_name(item.store),
                final_score = item.final_score,
                "long-term memory pre-injection candidate filtered by empty content"
            );
            continue;
        }
        if !dedupe.insert(key) {
            trace!(
                store = %memory_store_name(item.store),
                final_score = item.final_score,
                raw_similarity = item.raw_similarity,
                "long-term memory pre-injection candidate filtered by dedupe"
            );
            continue;
        }
        trace!(
            store = %memory_store_name(item.store),
            final_score = item.final_score,
            raw_similarity = item.raw_similarity,
            "long-term memory pre-injection candidate selected"
        );
        out.push(item);
        if out.len() >= normalized.top_k as usize {
            trace!(
                top_k = normalized.top_k,
                "long-term memory pre-injection reached top_k"
            );
            break;
        }
    }
    out
}

fn memory_store_name(store: MemoryHitStore) -> &'static str {
    match store {
        MemoryHitStore::LongTerm => "long-term",
    }
}

fn normalize_memory_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn select_forget_matches(
    rows: Vec<LongTermEmbeddedRow>,
    query_embedding: &[f32],
    similarity_threshold: f32,
    max_matches: usize,
) -> Vec<LongTermForgetMatch> {
    let mut matches = rows
        .into_iter()
        .filter_map(|row| {
            let emb = decode_f32_blob(&row.embedding);
            if emb.is_empty() {
                return None;
            }
            let similarity = cosine_similarity(query_embedding, &emb);
            if similarity < similarity_threshold {
                return None;
            }
            Some(LongTermForgetMatch {
                id: row.id,
                content: row.content,
                kind: row.kind,
                importance: row.importance,
                similarity,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(Ordering::Equal)
    });
    matches.truncate(max_matches);
    matches
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use crate::config::MemoryPreinjectConfig;

    use super::{
        MemoryHitStore, MemoryPreinjectHit, encode_f32_blob, has_recent_long_term_fact,
        rank_preinject_hits, select_forget_matches,
    };

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
            "general",
            "The squire loves bananas.",
            "2026-03-06T19:11:40.000000+00:00",
        )
        .expect("query should succeed");
        assert!(matched);

        let too_old = has_recent_long_term_fact(
            &conn,
            "general",
            "The squire loves bananas.",
            "2026-03-06T19:11:45.000000+00:00",
        )
        .expect("query should succeed");
        assert!(!too_old);
    }

    #[test]
    fn select_forget_matches_applies_threshold_and_cap() {
        let rows = vec![
            super::LongTermEmbeddedRow {
                id: 1,
                content: "A".to_owned(),
                embedding: encode_f32_blob(&[1.0, 0.0]),
                kind: "general".to_owned(),
                importance: 3,
            },
            super::LongTermEmbeddedRow {
                id: 2,
                content: "B".to_owned(),
                embedding: encode_f32_blob(&[0.9, 0.1]),
                kind: "general".to_owned(),
                importance: 3,
            },
            super::LongTermEmbeddedRow {
                id: 3,
                content: "C".to_owned(),
                embedding: encode_f32_blob(&[0.0, 1.0]),
                kind: "general".to_owned(),
                importance: 3,
            },
        ];

        let matches = select_forget_matches(rows, &[1.0, 0.0], 0.85, 2);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].id, 1);
        assert_eq!(matches[1].id, 2);
    }

    #[test]
    fn rank_preinject_hits_applies_threshold_dedupe_and_limit() {
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: 2,
            min_score: 0.75,
            max_items_per_store: 20,
            long_term_weight: 0.65,
            max_chars: 1200,
        };
        let hits = vec![
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Prefers short answers".to_owned(),
                kind: Some("prefs".to_owned()),
                importance: Some(5),
                raw_similarity: 0.94,
                final_score: 0.86,
            },
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Working in Rust project".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(3),
                raw_similarity: 0.89,
                final_score: 0.81,
            },
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Low confidence".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(1),
                raw_similarity: 0.5,
                final_score: 0.3,
            },
        ];

        let ranked = rank_preinject_hits(hits, &config);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].content, "Prefers short answers");
        assert_eq!(ranked[1].content, "Working in Rust project");
        assert!(ranked.iter().all(|hit| hit.raw_similarity >= 0.75));
    }

    #[test]
    fn rank_preinject_hits_filters_on_raw_similarity_not_final_score() {
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: 2,
            min_score: 0.72,
            max_items_per_store: 20,
            long_term_weight: 0.65,
            max_chars: 1200,
        };
        let hits = vec![
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "High weighted, low raw".to_owned(),
                kind: Some("prefs".to_owned()),
                importance: Some(5),
                raw_similarity: 0.70,
                final_score: 0.95,
            },
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Passes raw threshold".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(5),
                raw_similarity: 0.90,
                final_score: 0.60,
            },
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Also passes raw threshold".to_owned(),
                kind: Some("context".to_owned()),
                importance: Some(3),
                raw_similarity: 0.88,
                final_score: 0.58,
            },
        ];

        let ranked = rank_preinject_hits(hits, &config);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].content, "Passes raw threshold");
        assert_eq!(ranked[1].content, "Also passes raw threshold");
        assert!(ranked.iter().all(|hit| hit.raw_similarity >= 0.72));
    }
}
