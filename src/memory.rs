use std::cmp::Ordering;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chrono::Duration;
use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use fastembed::{InitOptions, TextEmbedding};
use rusqlite::params;
use tokio::sync::Mutex;
use tracing::trace;

use crate::config::{DatabaseConfig, EmbeddingConfig, MemoryPreinjectConfig};
use crate::error::FrameworkError;
use crate::paths::AppPaths;

#[derive(Clone)]
pub struct MemoryStore {
    pool: Pool,
    long_term_pool: Pool,
    embedder: Option<Arc<Mutex<TextEmbedding>>>,
}

const MEMORIZE_DEDUPE_WINDOW_SECS: i64 = 300;
const ALLOWED_MEMORY_KINDS: [&str; 6] = [
    "general",
    "profile",
    "preferences",
    "project",
    "task",
    "constraint",
];

#[derive(Debug, Clone)]
pub enum MemorizeResult {
    Inserted,
    Updated { superseded_content: String },
    Duplicate,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LongTermFactSummary {
    pub id: i64,
    pub content: String,
    pub kind: String,
    pub importance: i64,
    pub created_at: String,
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
        let paths = AppPaths::resolve()?;
        paths.ensure_fastembed_cache_dir()?;
        Self::new_with_cache_dir(
            short_term_path,
            long_term_path,
            db_config,
            _embedding_config,
            &paths.fastembed_cache_dir,
        )
        .await
    }

    pub(crate) async fn new_with_cache_dir(
        short_term_path: &Path,
        long_term_path: &Path,
        db_config: &DatabaseConfig,
        _embedding_config: &EmbeddingConfig,
        fastembed_cache_dir: &Path,
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

        std::fs::create_dir_all(fastembed_cache_dir)?;
        let mut embedder_options = InitOptions::default();
        embedder_options.cache_dir = fastembed_cache_dir.to_path_buf();
        let embedder = TextEmbedding::try_new(embedder_options)
            .map_err(|e| FrameworkError::Config(format!("failed to initialize embedder: {e}")))?;

        let this = Self {
            pool,
            long_term_pool,
            embedder: Some(Arc::new(Mutex::new(embedder))),
        };

        this.init_schema(db_config).await?;
        this.init_long_term_schema(db_config).await?;
        Ok(this)
    }

    pub(crate) async fn new_without_embedder(
        short_term_path: &Path,
        long_term_path: &Path,
        db_config: &DatabaseConfig,
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

        let this = Self {
            pool,
            long_term_pool,
            embedder: None,
        };

        this.init_schema(db_config).await?;
        this.init_long_term_schema(db_config).await?;
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

                CREATE VIRTUAL TABLE IF NOT EXISTS ltm_facts_vec USING vec0(
                    fact_id INTEGER PRIMARY KEY,
                    embedding float[384]
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

        Ok(())
    }

    pub async fn semantic_query_combined(
        &self,
        session_id: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<String>, FrameworkError> {
        let top_k = top_k.max(1);
        let top_k_u32 = u32::try_from(top_k).unwrap_or(u32::MAX);
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: top_k_u32,
            min_score: 0.0,
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
        let query_embedding = embed_text(self.embedder_ref()?, query).await?;
        let _ = session_id;

        let sql_limit = (config.top_k * 3).max(1) as i64;
        let query_blob = encode_f32_blob(&query_embedding);
        let long_term_conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        let candidates = long_term_conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT f.content, f.kind, f.importance, v.distance
                     FROM ltm_facts_vec v
                     JOIN ltm_facts f ON f.id = v.fact_id
                     WHERE v.embedding MATCH ?1
                       AND v.k = ?2
                     ORDER BY v.distance
                    ",
                )?;
                let mapped = stmt.query_map(params![query_blob, sql_limit], |row| {
                    let content: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    let importance: i64 = row.get(2)?;
                    let distance: f32 = row.get(3)?;
                    Ok((content, kind, importance, distance))
                })?;
                let mut out = Vec::new();
                for row in mapped {
                    let (content, kind, importance, distance) = row?;
                    // Convert L2 distance to cosine similarity (vectors are normalized)
                    let similarity = 1.0 - (distance * distance / 2.0);
                    let imp = (importance as f32).clamp(1.0, 5.0);
                    let with_importance = similarity * (1.0 + (imp - 1.0) * 0.1);
                    out.push(MemoryPreinjectHit {
                        store: MemoryHitStore::LongTerm,
                        content,
                        kind: Some(kind),
                        importance: Some(importance),
                        raw_similarity: similarity,
                        final_score: with_importance * config.long_term_weight,
                    });
                }
                Ok::<Vec<MemoryPreinjectHit>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

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
        let query_embedding = embed_text(self.embedder_ref()?, trimmed).await?;
        let normalized_kind = kind_filter
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_memory_kind)
            .transpose()?;

        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;

        // Fetch more candidates than needed to allow post-filtering by kind
        let fetch_limit = if normalized_kind.is_some() {
            (max_matches * 5) as i64
        } else {
            max_matches as i64
        };
        let query_blob = encode_f32_blob(&query_embedding);
        let kind_clone = normalized_kind.clone();

        let matches = conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT f.id, f.content, f.kind, f.importance, v.distance
                     FROM ltm_facts_vec v
                     JOIN ltm_facts f ON f.id = v.fact_id
                     WHERE v.embedding MATCH ?1
                       AND v.k = ?2
                     ORDER BY v.distance
                    ",
                )?;
                let mapped = stmt.query_map(params![query_blob, fetch_limit], |row| {
                    let id: i64 = row.get(0)?;
                    let content: String = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let importance: i64 = row.get(3)?;
                    let distance: f32 = row.get(4)?;
                    Ok((id, content, kind, importance, distance))
                })?;
                let mut out = Vec::new();
                for row in mapped {
                    let (id, content, kind, importance, distance) = row?;
                    let similarity = 1.0 - (distance * distance / 2.0);
                    if similarity < threshold {
                        continue;
                    }
                    if let Some(ref filter) = kind_clone {
                        if kind != *filter {
                            continue;
                        }
                    }
                    out.push(LongTermForgetMatch {
                        id,
                        content,
                        kind,
                        importance,
                        similarity,
                    });
                    if out.len() >= max_matches {
                        break;
                    }
                }
                Ok::<Vec<LongTermForgetMatch>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

        let mut deleted_count = 0usize;
        if commit && !matches.is_empty() {
            let ids = matches.iter().map(|item| item.id).collect::<Vec<_>>();
            deleted_count = conn
                .interact(move |conn| {
                    let tx = conn.transaction()?;
                    let mut count = 0usize;
                    for id in &ids {
                        count += tx.execute("DELETE FROM ltm_facts WHERE id = ?1", params![id])?;
                        tx.execute("DELETE FROM ltm_facts_vec WHERE fact_id = ?1", params![id])?;
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
    ) -> Result<MemorizeResult, FrameworkError> {
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
                parse_memory_kind(trimmed_kind)?
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
            return Ok(MemorizeResult::Duplicate);
        }

        let embedding = embed_text(self.embedder_ref()?, trimmed).await?;
        let blob = encode_f32_blob(&embedding);

        // Check for a semantically similar existing fact to supersede
        let supersede_blob = blob.clone();
        let supersede_kind = kind.clone();
        let existing = conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT f.id, f.content, v.distance
                     FROM ltm_facts_vec v
                     JOIN ltm_facts f ON f.id = v.fact_id
                     WHERE f.kind = ?1
                       AND v.embedding MATCH ?2
                       AND v.k = 1
                     ORDER BY v.distance",
                )?;
                let mut rows = stmt.query(params![supersede_kind, supersede_blob])?;
                if let Some(row) = rows.next()? {
                    let id: i64 = row.get(0)?;
                    let content: String = row.get(1)?;
                    let distance: f32 = row.get(2)?;
                    let similarity = 1.0 - (distance * distance / 2.0);
                    Ok::<Option<(i64, String, f32)>, rusqlite::Error>(Some((
                        id, content, similarity,
                    )))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))??;

        if let Some((existing_id, old_content, similarity)) = existing {
            if similarity >= 0.92 {
                // Supersede the existing fact
                let update_blob = blob;
                let update_fact = fact;
                let update_now = now;
                conn.interact(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute(
                        "UPDATE ltm_facts SET content = ?1, embedding = ?2, importance = ?3, created_at = ?4, source_session_id = ?5 WHERE id = ?6",
                        params![update_fact, update_blob, importance, update_now, session, existing_id],
                    )?;
                    tx.execute(
                        "DELETE FROM ltm_facts_vec WHERE fact_id = ?1",
                        params![existing_id],
                    )?;
                    tx.execute(
                        "INSERT INTO ltm_facts_vec (fact_id, embedding) VALUES (?1, ?2)",
                        params![existing_id, update_blob],
                    )?;
                    tx.commit()?;
                    Ok::<(), rusqlite::Error>(())
                })
                .await
                .map_err(|e| FrameworkError::Config(e.to_string()))??;

                return Ok(MemorizeResult::Updated {
                    superseded_content: old_content,
                });
            }
        }

        // Insert new fact
        let insert_blob = blob;
        conn.interact(move |conn| {
            conn.execute(
                "INSERT INTO ltm_facts (source_session_id, content, kind, importance, embedding, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session, fact, kind, importance, insert_blob, now],
            )?;
            let row_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO ltm_facts_vec (fact_id, embedding) VALUES (?1, ?2)",
                params![row_id, insert_blob],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;

        Ok(MemorizeResult::Inserted)
    }

    pub async fn list_long_term_facts(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LongTermFactSummary>, FrameworkError> {
        let limit = limit.clamp(1, 200) as i64;
        let kind = kind_filter
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(parse_memory_kind)
            .transpose()?;
        let conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        conn.interact(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, content, kind, importance, created_at FROM ltm_facts
                 WHERE (?1 IS NULL OR kind = ?1)
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![kind, limit], |row| {
                Ok(LongTermFactSummary {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    kind: row.get(2)?,
                    importance: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            Ok::<Vec<LongTermFactSummary>, rusqlite::Error>(out)
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))?
        .map_err(|e| FrameworkError::Config(e.to_string()))
    }
}

impl MemoryStore {
    fn embedder_ref(&self) -> Result<&Arc<Mutex<TextEmbedding>>, FrameworkError> {
        self.embedder.as_ref().ok_or_else(|| {
            FrameworkError::Config(
                "embedding model is unavailable; semantic memory features are disabled".to_owned(),
            )
        })
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
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut i8,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(
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

pub(crate) fn normalize_memory_kind(value: &str) -> Option<String> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    if normalized.is_empty() {
        return None;
    }
    match normalized.as_str() {
        "general" => Some("general".to_owned()),
        "profile" => Some("profile".to_owned()),
        "preferences" => Some("preferences".to_owned()),
        "project" => Some("project".to_owned()),
        "task" => Some("task".to_owned()),
        "constraint" => Some("constraint".to_owned()),
        _ => None,
    }
}

fn parse_memory_kind(value: &str) -> Result<String, FrameworkError> {
    normalize_memory_kind(value).ok_or_else(|| {
        FrameworkError::Tool(format!(
            "invalid memory kind '{value}'. allowed kinds: {}",
            ALLOWED_MEMORY_KINDS.join("|")
        ))
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use crate::config::MemoryPreinjectConfig;

    use super::{
        MemoryHitStore, MemoryPreinjectHit, has_recent_long_term_fact, normalize_memory_kind,
        parse_memory_kind, rank_preinject_hits,
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
    fn rank_preinject_hits_applies_threshold_dedupe_and_limit() {
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: 2,
            min_score: 0.75,
            long_term_weight: 0.65,
            max_chars: 1200,
        };
        let hits = vec![
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "Prefers short answers".to_owned(),
                kind: Some("preferences".to_owned()),
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
            long_term_weight: 0.65,
            max_chars: 1200,
        };
        let hits = vec![
            MemoryPreinjectHit {
                store: MemoryHitStore::LongTerm,
                content: "High weighted, low raw".to_owned(),
                kind: Some("preferences".to_owned()),
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

    #[test]
    fn normalize_memory_kind_accepts_only_canonical_values() {
        assert_eq!(normalize_memory_kind("preferences"), Some("preferences".to_owned()));
        assert_eq!(normalize_memory_kind("task"), Some("task".to_owned()));
        assert_eq!(normalize_memory_kind("prefs"), None);
        assert_eq!(normalize_memory_kind(""), None);
    }

    #[test]
    fn parse_memory_kind_returns_error_for_unknown_values() {
        assert!(parse_memory_kind("project").is_ok());
        assert!(parse_memory_kind("repo").is_err());
    }
}
