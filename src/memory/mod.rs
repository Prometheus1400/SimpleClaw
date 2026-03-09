use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Duration;
use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use fastembed::{InitOptions, TextEmbedding};
use rusqlite::params;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::{DatabaseConfig, EmbeddingConfig, MemoryRecallConfig};
use crate::error::FrameworkError;
use crate::paths::AppPaths;

mod embedder;
mod recall;
mod schema;
mod types;

use embedder::{embed_text, encode_f32_blob};
use recall::{parse_memory_kind, rank_recall_hits};
use schema::register_sqlite_vec;
pub use types::{
    LongTermFactSummary, LongTermForgetMatch, LongTermForgetResult, MemorizeResult, MemoryHitStore,
    MemoryRecallHit, MemoryStoreScope, StoredMessage, StoredRole,
};

pub type DynMemory = Arc<dyn Memory>;

#[async_trait]
pub trait Memory: Send + Sync {
    async fn append_message(
        &self,
        session_id: &str,
        role: StoredRole,
        content: &str,
        username: Option<&str>,
    ) -> Result<(), FrameworkError>;
    async fn semantic_query_combined(
        &self,
        session_id: &str,
        query: &str,
        top_k: usize,
        history_window: usize,
        scope: MemoryStoreScope,
    ) -> Result<Vec<String>, FrameworkError>;
    async fn query_recall_hits(
        &self,
        session_id: &str,
        query: &str,
        config: &MemoryRecallConfig,
        history_window: usize,
        scope: MemoryStoreScope,
        prefer_long_term: bool,
    ) -> Result<Vec<MemoryRecallHit>, FrameworkError>;
    async fn semantic_forget_long_term(
        &self,
        query: &str,
        similarity_threshold: f32,
        max_matches: usize,
        kind_filter: Option<&str>,
        commit: bool,
    ) -> Result<LongTermForgetResult, FrameworkError>;
    async fn recent_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, FrameworkError>;
    async fn memorize(
        &self,
        session_id: &str,
        content: &str,
        kind: &str,
        importance: u8,
    ) -> Result<MemorizeResult, FrameworkError>;
    async fn list_long_term_facts(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LongTermFactSummary>, FrameworkError>;
}

#[derive(Clone)]
pub struct MemoryStore {
    pool: Pool,
    long_term_pool: Pool,
    embedder: Option<Arc<Mutex<TextEmbedding>>>,
}

const MEMORIZE_DEDUPE_WINDOW_SECS: i64 = 300;

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

                CREATE VIRTUAL TABLE IF NOT EXISTS messages_vec USING vec0(
                    message_id INTEGER PRIMARY KEY,
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
        role: StoredRole,
        content: &str,
        username: Option<&str>,
    ) -> Result<(), FrameworkError> {
        debug!(
            status = "started",
            session_id = %session_id,
            "memory append_message"
        );
        let now = chrono::Utc::now().to_rfc3339();
        let session_for_sessions = session_id.to_owned();
        let session_for_messages = session_id.to_owned();
        let should_index_short_term = matches!(role, StoredRole::User | StoredRole::Assistant);
        let role = role.as_db_str().to_owned();
        let content_owned = content.to_owned();
        let username_owned = username
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let now_for_session = now.clone();
        let now_for_message = now;
        let message_embedding = if should_index_short_term {
            match self.embedder.as_ref() {
                Some(embedder) => match embed_text(embedder, content).await {
                    Ok(values) => Some(encode_f32_blob(&values)),
                    Err(err) => {
                        warn!(
                            status = "degraded",
                            session_id = %session_id,
                            error = %err,
                            "memory short-term embedding failed; continuing without vector index"
                        );
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

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
            if let Some(embedding) = message_embedding {
                let row_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT OR REPLACE INTO messages_vec (message_id, embedding) VALUES (?1, ?2)",
                    params![row_id, embedding],
                )?;
            }
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))??;
        debug!(
            status = "completed",
            session_id = %session_id,
            "memory append_message"
        );

        Ok(())
    }

    pub async fn semantic_query_combined(
        &self,
        session_id: &str,
        query: &str,
        top_k: usize,
        history_window: usize,
        scope: MemoryStoreScope,
    ) -> Result<Vec<String>, FrameworkError> {
        let top_k = top_k.max(1);
        let top_k_u32 = u32::try_from(top_k).unwrap_or(u32::MAX);
        let config = MemoryRecallConfig {
            enabled: true,
            top_k: top_k_u32,
            min_score: 0.0,
            long_term_weight: 1.0,
            max_chars: 4000,
        };
        let hits = self
            .query_recall_hits(session_id, query, &config, history_window, scope, false)
            .await?;
        Ok(hits
            .into_iter()
            .map(|hit| match hit.store {
                MemoryHitStore::LongTerm => format!(
                    "[long-term/{}] {}",
                    hit.kind.as_deref().unwrap_or("general"),
                    hit.content
                ),
                MemoryHitStore::ShortTerm => format!(
                    "[short-term/{}] {}",
                    hit.kind.as_deref().unwrap_or("message"),
                    hit.content
                ),
            })
            .collect())
    }

    pub async fn query_recall_hits(
        &self,
        session_id: &str,
        query: &str,
        config: &MemoryRecallConfig,
        history_window: usize,
        scope: MemoryStoreScope,
        prefer_long_term: bool,
    ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
        let started = std::time::Instant::now();
        debug!(
            status = "started",
            session_id = %session_id,
            "memory recall query"
        );
        let config = config.normalized();
        let query_embedding = embed_text(self.embedder_ref()?, query).await?;
        let sql_limit = (config.top_k * 5).max(1) as i64;
        let query_blob = encode_f32_blob(&query_embedding);
        let mut long_term_hits = Vec::new();
        let mut short_term_hits = Vec::new();

        if matches!(
            scope,
            MemoryStoreScope::Combined | MemoryStoreScope::LongTerm
        ) {
            long_term_hits = self
                .fetch_long_term_candidates(query_blob.clone(), sql_limit, config.long_term_weight)
                .await?;
        }
        if matches!(
            scope,
            MemoryStoreScope::Combined | MemoryStoreScope::ShortTerm
        ) {
            short_term_hits = self
                .fetch_short_term_candidates(session_id, query_blob, sql_limit, history_window)
                .await?;
        }

        let hits = if prefer_long_term && matches!(scope, MemoryStoreScope::Combined) {
            let mut out = Vec::new();
            let mut ranked_long = rank_recall_hits(long_term_hits, &config);
            let mut ranked_short = rank_recall_hits(short_term_hits, &config);
            out.append(&mut ranked_long);
            if out.len() < config.top_k as usize {
                let remaining = config.top_k as usize - out.len();
                out.extend(ranked_short.drain(..remaining.min(ranked_short.len())));
            }
            out
        } else {
            let mut candidates = Vec::with_capacity(long_term_hits.len() + short_term_hits.len());
            candidates.extend(long_term_hits);
            candidates.extend(short_term_hits);
            rank_recall_hits(candidates, &config)
        };
        debug!(
            status = "completed",
            session_id = %session_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "memory recall query"
        );
        Ok(hits)
    }

    async fn fetch_long_term_candidates(
        &self,
        query_blob: Vec<u8>,
        sql_limit: i64,
        long_term_weight: f32,
    ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
        let long_term_conn = self
            .long_term_pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        long_term_conn
            .interact(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT f.content, f.kind, f.importance, v.distance
                     FROM ltm_facts_vec v
                     JOIN ltm_facts f ON f.id = v.fact_id
                     WHERE v.embedding MATCH ?1
                       AND v.k = ?2
                     ORDER BY v.distance",
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
                    let similarity = 1.0 - (distance * distance / 2.0);
                    let imp = (importance as f32).clamp(1.0, 5.0);
                    let with_importance = similarity * (1.0 + (imp - 1.0) * 0.1);
                    out.push(MemoryRecallHit {
                        store: MemoryHitStore::LongTerm,
                        content,
                        kind: Some(kind),
                        importance: Some(importance),
                        raw_similarity: similarity,
                        final_score: with_importance * long_term_weight,
                    });
                }
                Ok::<Vec<MemoryRecallHit>, rusqlite::Error>(out)
            })
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?
            .map_err(|e| FrameworkError::Config(e.to_string()))
    }

    async fn fetch_short_term_candidates(
        &self,
        session_id: &str,
        query_blob: Vec<u8>,
        sql_limit: i64,
        history_window: usize,
    ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
        let session = session_id.to_owned();
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| FrameworkError::Config(e.to_string()))?;
        conn.interact(move |conn| {
            let excluded = fetch_recent_short_term_ids(conn, &session, history_window)?;
            let mut stmt = conn.prepare(
                "SELECT m.id, m.role, m.content, m.username, v.distance
                 FROM messages_vec v
                 JOIN messages m ON m.id = v.message_id
                 WHERE m.session_id = ?1
                   AND (m.role = 'user' OR m.role = 'assistant')
                   AND v.embedding MATCH ?2
                   AND v.k = ?3
                 ORDER BY v.distance",
            )?;
            let mapped = stmt.query_map(params![session, query_blob, sql_limit], |row| {
                let id: i64 = row.get(0)?;
                let role: String = row.get(1)?;
                let content: String = row.get(2)?;
                let username: Option<String> = row.get(3)?;
                let distance: f32 = row.get(4)?;
                Ok((id, role, content, username, distance))
            })?;

            let mut out = Vec::new();
            for row in mapped {
                let (id, role, content, username, distance) = row?;
                if excluded.contains(&id) {
                    continue;
                }
                let similarity = 1.0 - (distance * distance / 2.0);
                let rendered_content = if role == "user" {
                    username
                        .map(|name| name.trim().to_owned())
                        .filter(|name| !name.is_empty())
                        .map(|name| format!("[{name}] {content}"))
                        .unwrap_or(content)
                } else {
                    content
                };
                out.push(MemoryRecallHit {
                    store: MemoryHitStore::ShortTerm,
                    content: rendered_content,
                    kind: Some(role),
                    importance: None,
                    raw_similarity: similarity,
                    final_score: similarity,
                });
            }
            Ok::<Vec<MemoryRecallHit>, rusqlite::Error>(out)
        })
        .await
        .map_err(|e| FrameworkError::Config(e.to_string()))?
        .map_err(|e| FrameworkError::Config(e.to_string()))
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
                    let role_raw: String = row.get(0)?;
                    let role = StoredRole::from_db_str(&role_raw).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            "invalid role".into(),
                        )
                    })?;
                    Ok::<StoredMessage, rusqlite::Error>(StoredMessage {
                        role,
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
        debug!(
            status = "completed",
            session_id = %session_id,
            "memory recent_messages"
        );
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

#[async_trait]
impl Memory for MemoryStore {
    async fn append_message(
        &self,
        session_id: &str,
        role: StoredRole,
        content: &str,
        username: Option<&str>,
    ) -> Result<(), FrameworkError> {
        MemoryStore::append_message(self, session_id, role, content, username).await
    }

    async fn semantic_query_combined(
        &self,
        session_id: &str,
        query: &str,
        top_k: usize,
        history_window: usize,
        scope: MemoryStoreScope,
    ) -> Result<Vec<String>, FrameworkError> {
        MemoryStore::semantic_query_combined(self, session_id, query, top_k, history_window, scope)
            .await
    }

    async fn query_recall_hits(
        &self,
        session_id: &str,
        query: &str,
        config: &MemoryRecallConfig,
        history_window: usize,
        scope: MemoryStoreScope,
        prefer_long_term: bool,
    ) -> Result<Vec<MemoryRecallHit>, FrameworkError> {
        MemoryStore::query_recall_hits(
            self,
            session_id,
            query,
            config,
            history_window,
            scope,
            prefer_long_term,
        )
        .await
    }

    async fn semantic_forget_long_term(
        &self,
        query: &str,
        similarity_threshold: f32,
        max_matches: usize,
        kind_filter: Option<&str>,
        commit: bool,
    ) -> Result<LongTermForgetResult, FrameworkError> {
        MemoryStore::semantic_forget_long_term(
            self,
            query,
            similarity_threshold,
            max_matches,
            kind_filter,
            commit,
        )
        .await
    }

    async fn recent_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, FrameworkError> {
        MemoryStore::recent_messages(self, session_id, limit).await
    }

    async fn memorize(
        &self,
        session_id: &str,
        content: &str,
        kind: &str,
        importance: u8,
    ) -> Result<MemorizeResult, FrameworkError> {
        MemoryStore::memorize(self, session_id, content, kind, importance).await
    }

    async fn list_long_term_facts(
        &self,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LongTermFactSummary>, FrameworkError> {
        MemoryStore::list_long_term_facts(self, kind_filter, limit).await
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

fn fetch_recent_short_term_ids(
    conn: &rusqlite::Connection,
    session_id: &str,
    history_window: usize,
) -> Result<HashSet<i64>, rusqlite::Error> {
    if history_window == 0 {
        return Ok(HashSet::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id
         FROM messages
         WHERE session_id = ?1
           AND (role = 'user' OR role = 'assistant')
         ORDER BY id DESC
         LIMIT ?2",
    )?;
    let mapped = stmt.query_map(params![session_id, history_window as i64], |row| row.get(0))?;
    let mut ids = HashSet::new();
    for row in mapped {
        ids.insert(row?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::{fetch_recent_short_term_ids, has_recent_long_term_fact};
    use rusqlite::Connection;

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
    fn recent_short_term_ids_returns_newest_user_and_assistant_only() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite should open");
        conn.execute_batch(
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                username TEXT,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .expect("messages table should be created");

        conn.execute(
            "INSERT INTO messages (session_id, role, content, username, created_at) VALUES ('s1','user','one','u','t')",
            [],
        )
        .expect("insert should succeed");
        conn.execute(
            "INSERT INTO messages (session_id, role, content, username, created_at) VALUES ('s1','tool','ignore',NULL,'t')",
            [],
        )
        .expect("insert should succeed");
        conn.execute(
            "INSERT INTO messages (session_id, role, content, username, created_at) VALUES ('s1','assistant','two',NULL,'t')",
            [],
        )
        .expect("insert should succeed");
        conn.execute(
            "INSERT INTO messages (session_id, role, content, username, created_at) VALUES ('s1','user','three','u','t')",
            [],
        )
        .expect("insert should succeed");

        let ids = fetch_recent_short_term_ids(&conn, "s1", 2).expect("query should succeed");
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&4));
        assert!(ids.contains(&3));
    }
}
