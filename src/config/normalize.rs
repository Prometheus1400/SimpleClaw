use std::env;
use std::path::{Path, PathBuf};

use super::agents::AgentsConfig;
use super::agents::AgentEntryConfig;

pub(super) fn normalize_agents_workspace_paths(agents: &mut AgentsConfig) {
    agents.list = agents
        .list
        .iter()
        .map(|agent| AgentEntryConfig {
            id: agent.id.clone(),
            name: agent.name.clone(),
            workspace: normalize_workspace_path(&agent.workspace),
            runtime: agent.runtime.clone(),
        })
        .collect();
}

pub(super) fn normalize_workspace_path(path: &Path) -> PathBuf {
    let expanded = expand_env_vars(&path.to_string_lossy());
    expand_home_dir(&expanded).unwrap_or_else(|| PathBuf::from(expanded))
}

pub(super) fn expand_home_dir(value: &str) -> Option<PathBuf> {
    if !value.starts_with('~') {
        return None;
    }
    if value.len() > 1 {
        let separator = value.as_bytes()[1];
        if separator != b'/' && separator != b'\\' {
            return None;
        }
    }

    let home = home_dir()?;
    if value == "~" {
        return Some(home);
    }

    let mut full = home;
    let remainder = &value[2..];
    if !remainder.is_empty() {
        full.push(remainder);
    }
    Some(full)
}

pub(super) fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(super) fn expand_env_vars(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] != '$' {
            output.push(chars[i]);
            i += 1;
            continue;
        }

        if i + 1 >= chars.len() {
            output.push('$');
            i += 1;
            continue;
        }

        if chars[i + 1] == '{' {
            let mut end = i + 2;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            if end < chars.len() {
                let key: String = chars[i + 2..end].iter().collect();
                if is_valid_env_key(&key) {
                    if let Ok(value) = env::var(&key) {
                        output.push_str(&value);
                    } else {
                        output.push_str(&format!("${{{key}}}"));
                    }
                    i = end + 1;
                    continue;
                }
            }
            output.push('$');
            i += 1;
            continue;
        }

        let mut end = i + 1;
        while end < chars.len() && is_env_key_char(chars[end], end == i + 1) {
            end += 1;
        }
        if end == i + 1 {
            output.push('$');
            i += 1;
            continue;
        }

        let key: String = chars[i + 1..end].iter().collect();
        if let Ok(value) = env::var(&key) {
            output.push_str(&value);
        } else {
            output.push('$');
            output.push_str(&key);
        }
        i = end;
    }

    output
}

fn is_valid_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    if !is_env_key_char(chars.next().unwrap_or('_'), true) {
        return false;
    }
    chars.all(|ch| is_env_key_char(ch, false))
}

fn is_env_key_char(ch: char, first: bool) -> bool {
    if first {
        ch == '_' || ch.is_ascii_alphabetic()
    } else {
        ch == '_' || ch.is_ascii_alphanumeric()
    }
}
