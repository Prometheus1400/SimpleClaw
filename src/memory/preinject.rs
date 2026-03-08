use std::cmp::Ordering;

use tracing::trace;

use crate::config::MemoryPreinjectConfig;
use crate::error::FrameworkError;

use super::types::MemoryPreinjectHit;

const ALLOWED_MEMORY_KINDS: [&str; 6] = [
    "general",
    "profile",
    "preferences",
    "project",
    "task",
    "constraint",
];

pub(super) fn rank_preinject_hits(
    mut candidates: Vec<MemoryPreinjectHit>,
    normalized_config: &MemoryPreinjectConfig,
) -> Vec<MemoryPreinjectHit> {
    candidates.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(Ordering::Equal)
    });

    let mut dedupe = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in candidates {
        if item.raw_similarity < normalized_config.min_score {
            trace!(
                status = "filtered_by_min_score",
                "memory preinject candidate"
            );
            continue;
        }
        let key = normalize_memory_key(&item.content);
        if key.is_empty() {
            trace!(
                status = "filtered_empty_content",
                "memory preinject candidate"
            );
            continue;
        }
        if !dedupe.insert(key) {
            trace!(status = "filtered_dedupe", "memory preinject candidate");
            continue;
        }
        trace!(status = "selected", "memory preinject candidate");
        out.push(item);
        if out.len() >= normalized_config.top_k as usize {
            trace!(status = "top_k_reached", "memory preinject candidate");
            break;
        }
    }
    out
}

fn normalize_memory_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(super) fn normalize_memory_kind(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
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

pub(super) fn parse_memory_kind(value: &str) -> Result<String, FrameworkError> {
    normalize_memory_kind(value).ok_or_else(|| {
        FrameworkError::Tool(format!(
            "invalid memory kind '{value}'. allowed kinds: {}",
            ALLOWED_MEMORY_KINDS.join("|")
        ))
    })
}

#[cfg(test)]
mod tests {
    use crate::config::MemoryPreinjectConfig;

    use super::{normalize_memory_kind, parse_memory_kind, rank_preinject_hits};
    use crate::memory::{MemoryHitStore, MemoryPreinjectHit};

    #[test]
    fn rank_preinject_hits_applies_threshold_dedupe_and_limit() {
        let config = MemoryPreinjectConfig {
            enabled: true,
            top_k: 2,
            min_score: 0.75,
            long_term_weight: 0.65,
            max_chars: 1200,
        }
        .normalized();
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
        }
        .normalized();
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
        assert_eq!(
            normalize_memory_kind("preferences"),
            Some("preferences".to_owned())
        );
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
