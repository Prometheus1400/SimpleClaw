use tracing::{debug, warn};

use crate::agent::AgentRuntimeConfig;
use crate::channels::InboundMessage;
use crate::error::FrameworkError;
use crate::memory::MemoryStoreScope;
use crate::memory::{Memory, MemoryHitStore, MemoryRecallHit, ShortTermContextMessage, StoredRole};
use crate::providers::{Message, Role};

pub(super) struct PreparedTurn {
    pub system_prompt: String,
    pub history: Vec<Message>,
    pub memory_recall_short_hits: usize,
    pub memory_recall_long_hits: usize,
}

pub(super) async fn prepare_turn(
    agent: &AgentRuntimeConfig,
    memory: &dyn Memory,
    inbound: &InboundMessage,
    session_id: &str,
) -> Result<PreparedTurn, FrameworkError> {
    memory
        .append_message(
            session_id,
            StoredRole::User,
            &inbound.content,
            Some(&inbound.username),
        )
        .await?;

    let history = seeded_history(agent, memory, session_id).await?;
    let prompt_build = build_turn_system_prompt(agent, memory, session_id, &inbound.content).await;
    let system_prompt = inject_caller_context(&prompt_build.system_prompt, inbound);

    Ok(PreparedTurn {
        system_prompt,
        history,
        memory_recall_short_hits: prompt_build.memory_recall_short_hits,
        memory_recall_long_hits: prompt_build.memory_recall_long_hits,
    })
}

pub(super) async fn record_context(
    memory: &dyn Memory,
    inbound: &InboundMessage,
    session_id: &str,
) -> Result<(), FrameworkError> {
    memory
        .append_message(
            session_id,
            StoredRole::User,
            &inbound.content,
            Some(&inbound.username),
        )
        .await
}

struct PromptBuild {
    system_prompt: String,
    memory_recall_short_hits: usize,
    memory_recall_long_hits: usize,
}

impl PromptBuild {
    fn without_recall(system_prompt: String) -> Self {
        Self {
            system_prompt,
            memory_recall_short_hits: 0,
            memory_recall_long_hits: 0,
        }
    }
}

async fn seeded_history(
    agent: &AgentRuntimeConfig,
    memory: &dyn Memory,
    session_id: &str,
) -> Result<Vec<Message>, FrameworkError> {
    let history_limit = agent.effective_execution.history_messages as usize;
    let stored = memory.recent_messages(session_id, history_limit).await?;
    let mut history = Vec::with_capacity(stored.len());
    for item in stored {
        let role = match item.role {
            StoredRole::User => Role::User,
            StoredRole::Assistant => Role::Assistant,
            _ => continue,
        };
        let content = if matches!(role, Role::User) {
            if let Some(username) = item.username.as_deref().map(str::trim)
                && !username.is_empty()
            {
                format!("[{username}] {}", item.content)
            } else {
                item.content
            }
        } else {
            item.content
        };
        history.push(Message::text(role, content));
    }
    Ok(history)
}

async fn build_turn_system_prompt(
    agent: &AgentRuntimeConfig,
    memory: &dyn Memory,
    session_id: &str,
    query: &str,
) -> PromptBuild {
    let config = agent.effective_execution.memory_recall.normalized();
    if !config.enabled {
        return PromptBuild::without_recall(agent.system_prompt.clone());
    }

    let normalized_query = normalize_recall_query(query, &agent.agent_name, &agent.agent_id);
    if normalized_query.is_empty()
        || count_recall_words(&normalized_query) < config.recall_word_count_threshold as usize
    {
        return PromptBuild::without_recall(agent.system_prompt.clone());
    }

    let hits = match memory
        .query_recall_hits(
            session_id,
            &normalized_query,
            &config,
            agent.effective_execution.history_messages as usize,
            MemoryStoreScope::Combined,
            true,
        )
        .await
    {
        Ok(items) => items,
        Err(err) => {
            warn!(
                status = "failed",
                error_kind = "memory_recall_query",
                error = %err,
                "memory recall query"
            );
            return PromptBuild::without_recall(agent.system_prompt.clone());
        }
    };

    if hits.is_empty() {
        debug!(status = "completed", "memory recall");
        return PromptBuild::without_recall(agent.system_prompt.clone());
    }

    debug!(status = "completed", "memory recall");
    let recalled = format_recalled_memory(
        &hits,
        config.long_term_max_chars as usize,
        config.short_term_max_chars as usize,
    );
    if recalled.section.is_empty() {
        return PromptBuild::without_recall(agent.system_prompt.clone());
    }

    PromptBuild {
        system_prompt: format!("{}\n\n{}", agent.system_prompt, recalled.section),
        memory_recall_short_hits: recalled.short_hits,
        memory_recall_long_hits: recalled.long_hits,
    }
}

#[derive(Default)]
struct PromptBuildMemorySection {
    section: String,
    short_hits: usize,
    long_hits: usize,
}

fn format_recalled_memory(
    hits: &[MemoryRecallHit],
    long_term_max_chars: usize,
    short_term_max_chars: usize,
) -> PromptBuildMemorySection {
    if hits.is_empty() || (long_term_max_chars == 0 && short_term_max_chars == 0) {
        return PromptBuildMemorySection::default();
    }

    let long_hits = hits
        .iter()
        .filter(|hit| matches!(hit.store, MemoryHitStore::LongTerm))
        .cloned()
        .collect::<Vec<_>>();
    let short_hits = hits
        .iter()
        .filter(|hit| matches!(hit.store, MemoryHitStore::ShortTerm))
        .cloned()
        .collect::<Vec<_>>();

    let long_section = build_long_term_memory_section(&long_hits, long_term_max_chars);
    let short_section = build_short_term_memory_section(&short_hits, short_term_max_chars);
    join_memory_sections(&[long_section, short_section])
}

fn build_long_term_memory_section(
    hits: &[MemoryRecallHit],
    max_chars: usize,
) -> PromptBuildMemorySection {
    if hits.is_empty() || max_chars == 0 {
        return PromptBuildMemorySection::default();
    }

    let base = "# REMEMBERED FACTS\nPersistent facts from long-term memory. Prioritize the current conversation over these.";
    let mut section = base.to_owned();
    let mut long_hits = 0;
    for (index, hit) in hits.iter().enumerate() {
        let line = format!(
            "\n{}. [{}] {}",
            index + 1,
            hit.kind.as_deref().unwrap_or("general"),
            hit.content.trim()
        );
        if section.len() + line.len() > max_chars {
            break;
        }
        section.push_str(&line);
        long_hits += 1;
    }

    if long_hits == 0 {
        PromptBuildMemorySection::default()
    } else {
        PromptBuildMemorySection {
            section,
            short_hits: 0,
            long_hits,
        }
    }
}

fn build_short_term_memory_section(
    hits: &[MemoryRecallHit],
    max_chars: usize,
) -> PromptBuildMemorySection {
    if hits.is_empty() || max_chars == 0 {
        return PromptBuildMemorySection::default();
    }

    let base =
        "# RECALLED CONVERSATIONS\nExcerpts from earlier in this session that may be relevant.";
    let mut section = base.to_owned();
    let mut short_hits = 0;
    for hit in hits {
        let excerpt = render_short_term_excerpt(hit);
        if section.len() + excerpt.len() > max_chars {
            break;
        }
        section.push_str(&excerpt);
        short_hits += 1;
    }

    if short_hits == 0 {
        PromptBuildMemorySection::default()
    } else {
        PromptBuildMemorySection {
            section,
            short_hits,
            long_hits: 0,
        }
    }
}

fn render_short_term_excerpt(hit: &MemoryRecallHit) -> String {
    let mut excerpt = format!(
        "\n\n--- excerpt (similarity: {:.2}) ---",
        hit.raw_similarity
    );
    if let Some(messages) = hit.context_messages.as_ref() {
        for message in messages {
            excerpt.push('\n');
            excerpt.push_str(&render_context_message(message));
        }
    } else {
        excerpt.push_str(&format!("\n{}", hit.content.trim()));
    }
    excerpt.push_str("\n---");
    excerpt
}

fn render_context_message(message: &ShortTermContextMessage) -> String {
    match message.role {
        StoredRole::User => {
            let speaker = message
                .username
                .as_deref()
                .map(str::trim)
                .filter(|username| !username.is_empty())
                .unwrap_or("user");
            format!("[{speaker}]: {}", message.content.trim())
        }
        StoredRole::Assistant => format!("[assistant]: {}", message.content.trim()),
        StoredRole::System => format!("[system]: {}", message.content.trim()),
        StoredRole::Tool => format!("[tool]: {}", message.content.trim()),
    }
}

fn join_memory_sections(sections: &[PromptBuildMemorySection]) -> PromptBuildMemorySection {
    let mut joined = PromptBuildMemorySection::default();
    for section in sections {
        if section.section.is_empty() {
            continue;
        }
        joined.section = if joined.section.is_empty() {
            section.section.clone()
        } else {
            format!("{}\n\n{}", joined.section, section.section)
        };
        joined.short_hits += section.short_hits;
        joined.long_hits += section.long_hits;
    }
    joined
}

fn normalize_recall_query(query: &str, agent_name: &str, agent_id: &str) -> String {
    let mut remaining = query.trim();
    if remaining.is_empty() {
        return String::new();
    }

    loop {
        let next = strip_leading_discord_mention(remaining)
            .or_else(|| strip_leading_at_name(remaining, agent_name))
            .or_else(|| strip_leading_name_token(remaining, agent_name))
            .or_else(|| strip_leading_name_token(remaining, agent_id));
        let Some(stripped) = next else {
            break;
        };
        if stripped == remaining {
            break;
        }
        remaining = stripped.trim_start_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, ':' | ',' | '!' | '?' | '.' | ';' | '-')
        });
        if remaining.is_empty() {
            break;
        }
    }

    remaining.trim().to_owned()
}

fn strip_leading_discord_mention(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("<@")?;
    let close = rest.find('>')?;
    Some(&rest[close + 1..])
}

fn strip_leading_at_name<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return None;
    }
    let rest = value.strip_prefix('@')?;
    strip_leading_name_token(rest, trimmed_name)
}

fn strip_leading_name_token<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return None;
    }
    let prefix_len = starts_with_name_token(value, trimmed_name)?;
    Some(&value[prefix_len..])
}

fn starts_with_name_token(value: &str, name: &str) -> Option<usize> {
    if value.len() < name.len() || !value[..name.len()].eq_ignore_ascii_case(name) {
        return None;
    }
    if value.len() == name.len() {
        return Some(name.len());
    }
    let next = value[name.len()..].chars().next()?;
    if next.is_whitespace() || matches!(next, ':' | ',' | '!' | '?' | '.') {
        Some(name.len())
    } else {
        None
    }
}

fn count_recall_words(value: &str) -> usize {
    value.split_whitespace().count()
}

fn inject_caller_context(base: &str, inbound: &InboundMessage) -> String {
    let chat_type = if inbound.is_dm { "dm" } else { "group" };
    let platform = inbound.source_channel.as_str();
    let trigger_line = if inbound.user_id == "system" && inbound.username == "cron" {
        "\ntrigger: scheduled_cron"
    } else {
        ""
    };
    let guild_line = inbound
        .guild_id
        .as_ref()
        .map(|gid| format!("\nguild_id: {gid}"))
        .unwrap_or_default();
    let message_line = inbound
        .source_message_id
        .as_ref()
        .map(|message_id| format!("\nmessage_id: {message_id}"))
        .unwrap_or_default();
    format!(
        "{base}\n\n# CURRENT CONTEXT\nchat_type: {chat_type}\nplatform: {platform}\nchannel_id: {}{guild_line}{message_line}{trigger_line}\nSpeaker: **{}** (id: {})",
        inbound.channel_id, inbound.username, inbound.user_id
    )
}
