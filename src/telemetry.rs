//! Shared tracing helpers and conventions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TRACE_COUNTER: AtomicU64 = AtomicU64::new(1);
const REDACTED: &str = "***REDACTED***";

/// Generate a globally unique trace id string for correlating logs.
pub fn next_trace_id() -> String {
    let seq = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis())
        .unwrap_or(0);
    format!("trc-{now_ms}-{seq}")
}

/// Build a normalized, redacted single-line preview suitable for logs.
pub fn sanitize_preview(text: &str, max_chars: usize) -> String {
    truncate_for_log(
        &normalize_for_log_line(&redact_sensitive_values(text)),
        max_chars,
    )
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let clipped = text.chars().take(max_chars).collect::<String>();
    format!("{clipped}...[truncated]")
}

fn normalize_for_log_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn redact_sensitive_values(text: &str) -> String {
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) {
        redact_json_value(&mut json);
        return json.to_string();
    }

    let mut redacted = text.to_owned();
    for key in [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "access_token",
        "refresh_token",
    ] {
        redacted = redact_after_key(&redacted, key, '=');
        redacted = redact_after_key(&redacted, key, ':');
    }
    redact_bearer_token(&redacted)
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *child = serde_json::Value::String(REDACTED.to_owned());
                } else {
                    redact_json_value(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "access_token",
        "refresh_token",
    ]
    .iter()
    .any(|candidate| lower.contains(candidate))
}

fn redact_after_key(input: &str, key: &str, separator: char) -> String {
    let needle = format!("{key}{separator}");
    let mut output = input.to_owned();
    let mut search_from = 0usize;

    loop {
        if search_from >= output.len() {
            break;
        }
        let lower = output.to_ascii_lowercase();
        let Some(relative_idx) = lower[search_from..].find(&needle) else {
            break;
        };
        let token_start = search_from + relative_idx;
        let value_start = token_start + needle.len();
        if value_start >= output.len() {
            break;
        }
        let mut value_end = output[value_start..]
            .find(is_secret_value_terminator)
            .map(|idx| value_start + idx)
            .unwrap_or(output.len());

        if key.eq_ignore_ascii_case("authorization")
            && output[value_start..]
                .to_ascii_lowercase()
                .starts_with("bearer ")
        {
            let bearer_token_start = value_start + "bearer ".len();
            value_end = output[bearer_token_start..]
                .find(is_secret_value_terminator)
                .map(|idx| bearer_token_start + idx)
                .unwrap_or(output.len());
        }

        if value_end == value_start {
            search_from = value_start + 1;
            continue;
        }

        output.replace_range(value_start..value_end, REDACTED);
        search_from = value_start + REDACTED.len();
    }

    output
}

fn redact_bearer_token(input: &str) -> String {
    let mut output = input.to_owned();
    let mut search_from = 0usize;
    let needle = "bearer ";

    loop {
        if search_from >= output.len() {
            break;
        }
        let lower = output.to_ascii_lowercase();
        let Some(relative_idx) = lower[search_from..].find(needle) else {
            break;
        };
        let token_start = search_from + relative_idx;
        let value_start = token_start + needle.len();
        if value_start >= output.len() {
            break;
        }
        let value_end = output[value_start..]
            .find(is_secret_value_terminator)
            .map(|idx| value_start + idx)
            .unwrap_or(output.len());
        if value_end == value_start {
            search_from = value_start + 1;
            continue;
        }

        output.replace_range(value_start..value_end, REDACTED);
        search_from = value_start + REDACTED.len();
    }

    output
}

fn is_secret_value_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | ',' | '&' | ';' | ')' | '(' | ']' | '[' | '{' | '}'
        )
}

