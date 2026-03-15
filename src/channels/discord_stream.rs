use std::time::Instant;

use serenity::http::Http;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Duration;

use super::discord::{parse_channel_id, parse_message_id};
use crate::channels::ChannelStream;
use crate::error::FrameworkError;
use serenity::builder::EditMessage;

const STREAMING_EDIT_INTERVAL: Duration = Duration::from_millis(1_500);
const TOOL_STATUS_PREFIX: &str = "\n\n_\u{2699}\u{FE0F} ";
const TOOL_STATUS_SUFFIX: &str = "..._";

struct DiscordStreamingState {
    http: Arc<Http>,
    channel_id: String,
    channel_limit: usize,
    latest_content: String,
    committed_prefix_chars: usize,
    displayed_segment: Option<String>,
    message_id: Option<String>,
    last_edit: Instant,
    edit_count: usize,
    initial_send_attempted: bool,
    send_in_flight: bool,
    edit_in_flight: bool,
    finalized: bool,
    terminal_failure: bool,
    error_message: Option<String>,
    notify: Arc<Notify>,
    tool_status: Option<String>,
}

pub struct DiscordChannelStream {
    state: Arc<Mutex<DiscordStreamingState>>,
}

impl DiscordChannelStream {
    pub fn new(http: Arc<Http>, channel_id: &str, limit: usize) -> Self {
        let state = Arc::new(Mutex::new(DiscordStreamingState {
            http,
            channel_id: channel_id.to_owned(),
            channel_limit: limit,
            latest_content: String::new(),
            committed_prefix_chars: 0,
            displayed_segment: None,
            message_id: None,
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            edit_count: 0,
            initial_send_attempted: false,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
            tool_status: None,
        }));

        Self { state }
    }
}

#[async_trait::async_trait]
impl ChannelStream for DiscordChannelStream {
    fn push_delta(&self, delta: &str) {
        let mut state = self.state.lock().unwrap();
        state.latest_content.push_str(delta);
        state.notify.notify_waiters();
        drop(state);
        spawn_next_streaming_display_action(&self.state);
    }

    fn set_tool_status(&self, status: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.tool_status = status;
        state.notify.notify_waiters();
        drop(state);
        spawn_next_streaming_display_action(&self.state);
    }

    async fn finalize(&self, final_content: &str) -> Result<(), FrameworkError> {
        finalize_streaming_display(&self.state, final_content).await
    }
}
struct ActiveStreamingSegment {
    content: String,
    visible_chars: usize,
    has_overflow: bool,
}

fn byte_index_for_char_offset(content: &str, char_offset: usize) -> usize {
    if char_offset == 0 {
        return 0;
    }
    content
        .char_indices()
        .nth(char_offset)
        .map(|(idx, _)| idx)
        .unwrap_or(content.len())
}

fn active_streaming_segment(
    latest_content: &str,
    committed_prefix_chars: usize,
    channel_limit: Option<usize>,
    tool_status: Option<&str>,
) -> Option<ActiveStreamingSegment> {
    let start = byte_index_for_char_offset(latest_content, committed_prefix_chars);
    let tail = &latest_content[start..];
    if tail.is_empty() {
        return None;
    }

    let tail_chars = tail.chars().count();
    let tool_status_suffix = tool_status.and_then(|status| {
        let suffix = format!("{TOOL_STATUS_PREFIX}{status}{TOOL_STATUS_SUFFIX}");
        match channel_limit {
            Some(limit) if suffix.chars().count() >= limit => None,
            _ => Some(suffix),
        }
    });
    let reserved_chars = tool_status_suffix
        .as_ref()
        .map(|suffix| suffix.chars().count())
        .unwrap_or(0);
    let visible_chars = channel_limit.map_or(tail_chars, |limit| {
        limit.saturating_sub(reserved_chars).min(tail_chars)
    });
    let end = byte_index_for_char_offset(tail, visible_chars);
    let mut content = tail[..end].to_owned();
    if let Some(suffix) = tool_status_suffix {
        content.push_str(&suffix);
    }

    Some(ActiveStreamingSegment {
        content,
        visible_chars,
        has_overflow: tail_chars > visible_chars,
    })
}

fn try_rollover_streaming_segment(state: &mut DiscordStreamingState) -> bool {
    if state.send_in_flight || state.edit_in_flight || state.terminal_failure {
        return false;
    }
    let Some(segment) = active_streaming_segment(
        &state.latest_content,
        state.committed_prefix_chars,
        Some(state.channel_limit),
        state.tool_status.as_deref(),
    ) else {
        return false;
    };
    if !segment.has_overflow || state.displayed_segment.as_deref() != Some(segment.content.as_str())
    {
        return false;
    }

    state.committed_prefix_chars += segment.visible_chars;
    state.displayed_segment = None;
    state.message_id = None;
    state.initial_send_attempted = false;
    state.error_message = None;
    state.edit_count = 0;
    true
}

enum DiscordStreamingStateAction {
    SendInitial {
        http: Arc<Http>,
        channel_id: String,
        content: String,
    },
    Edit {
        http: Arc<Http>,
        channel_id: String,
        message_id: String,
        content: String,
    },
}

fn next_streaming_display_action(
    display: &Arc<Mutex<DiscordStreamingState>>,
) -> Option<DiscordStreamingStateAction> {
    let mut state = match display.lock() {
        Ok(state) => state,
        Err(_) => return None,
    };

    if state.terminal_failure || state.latest_content.is_empty() {
        return None;
    }

    if try_rollover_streaming_segment(&mut state) {
        state.notify.notify_waiters();
    }

    let segment = active_streaming_segment(
        &state.latest_content,
        state.committed_prefix_chars,
        Some(state.channel_limit),
        state.tool_status.as_deref(),
    )?;

    if state.message_id.is_none() {
        if state.send_in_flight || state.displayed_segment.is_some() || state.initial_send_attempted
        {
            return None;
        }
        state.initial_send_attempted = true;
        state.send_in_flight = true;
        return Some(DiscordStreamingStateAction::SendInitial {
            http: Arc::clone(&state.http),
            channel_id: state.channel_id.clone(),
            content: segment.content,
        });
    }

    if state.edit_in_flight {
        return None;
    }

    if state.displayed_segment.as_deref() == Some(segment.content.as_str()) {
        return None;
    }

    let current_interval = if state.edit_count < 3 {
        Duration::from_millis(500)
    } else {
        STREAMING_EDIT_INTERVAL
    };

    if !state.finalized && state.last_edit.elapsed() < current_interval {
        return None;
    }

    let message_id = state.message_id.clone()?;
    state.edit_in_flight = true;
    Some(DiscordStreamingStateAction::Edit {
        http: Arc::clone(&state.http),
        channel_id: state.channel_id.clone(),
        message_id,
        content: segment.content,
    })
}

fn spawn_next_streaming_display_action(display: &Arc<Mutex<DiscordStreamingState>>) {
    let Some(action) = next_streaming_display_action(display) else {
        return;
    };
    let display = Arc::clone(display);
    tokio::spawn(async move {
        match action {
            DiscordStreamingStateAction::SendInitial {
                http,
                channel_id,
                content,
            } => {
                let result: Result<String, FrameworkError> = async {
                    let parsed = parse_channel_id(&channel_id)?;
                    parsed
                        .say(&http, &content)
                        .await
                        .map(|msg| msg.id.get().to_string())
                        .map_err(|e: serenity::Error| FrameworkError::Tool(e.to_string()))
                }
                .await;
                let mut should_retry = false;
                {
                    let mut state = match display.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    state.send_in_flight = false;
                    match result {
                        Ok(message_id) => {
                            state.message_id = Some(message_id);
                            state.displayed_segment = Some(content);
                            state.last_edit = Instant::now();
                            state.error_message = None;
                            should_retry = true;
                        }

                        Err(err) => {
                            state.error_message = Some(err.to_string());
                            state.terminal_failure = true;
                            tracing::warn!(
                                status = "failed",
                                error_kind = "streaming_initial_send",
                                error = %err,
                                channel_id = %channel_id,

                                "streaming initial send failed"
                            );
                        }
                    }
                    state.notify.notify_waiters();
                }
                if should_retry {
                    spawn_next_streaming_display_action(&display);
                }
            }
            DiscordStreamingStateAction::Edit {
                http,
                channel_id,
                message_id,
                content,
            } => {
                let result: Result<(), FrameworkError> = async {
                    let parsed = parse_channel_id(&channel_id)?;
                    let parsed_msg = parse_message_id(&message_id)?;
                    parsed
                        .edit_message(&http, parsed_msg, EditMessage::new().content(&content))
                        .await
                        .map_err(|e: serenity::Error| FrameworkError::Tool(e.to_string()))
                        .map(|_| ())
                }
                .await;
                let mut should_retry = false;
                {
                    let mut state = match display.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    state.edit_in_flight = false;
                    if let Err(err) = result {
                        state.error_message = Some(err.to_string());
                        state.terminal_failure = true;
                        tracing::warn!(
                            status = "failed",
                            error_kind = "streaming_edit",
                            error = %err,
                            channel_id = %channel_id,

                            message_id = %message_id,
                            "streaming edit failed"
                        );
                    } else {
                        state.displayed_segment = Some(content);
                        state.last_edit = Instant::now();
                        state.edit_count += 1;
                        state.error_message = None;
                        should_retry = true;
                    }
                    state.notify.notify_waiters();
                }
                if should_retry {
                    spawn_next_streaming_display_action(&display);
                }
            }
        }
    });
}

async fn finalize_streaming_display(
    display: &Arc<Mutex<DiscordStreamingState>>,
    content: &str,
) -> Result<(), crate::error::FrameworkError> {
    {
        let mut state = display.lock().map_err(|_| {
            crate::error::FrameworkError::Tool("streaming display mutex poisoned".to_owned())
        })?;
        state.latest_content = content.to_owned();
        state.finalized = true;
        state.notify.notify_waiters();
    }
    spawn_next_streaming_display_action(display);

    loop {
        let fallback = {
            let state = display.lock().map_err(|_| {
                crate::error::FrameworkError::Tool("streaming display mutex poisoned".to_owned())
            })?;
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.terminal_failure
                && let Some(error_message) = state.error_message.clone()
            {
                if state.message_id.is_some()
                    || state.displayed_segment.is_some()
                    || state.committed_prefix_chars > 0
                {
                    return Err(crate::error::FrameworkError::Tool(error_message));
                }
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.committed_prefix_chars == content.chars().count()
            {
                return Ok(());
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && active_streaming_segment(
                    &state.latest_content,
                    state.committed_prefix_chars,
                    Some(state.channel_limit),
                    None,
                )
                .map(|segment| state.displayed_segment.as_deref() == Some(segment.content.as_str()))
                .unwrap_or(false)
                && state.committed_prefix_chars
                    + active_streaming_segment(
                        &state.latest_content,
                        state.committed_prefix_chars,
                        Some(state.channel_limit),
                        None,
                    )
                    .map(|segment| segment.visible_chars)
                    .unwrap_or(0)
                    == content.chars().count()
            {
                return Ok(());
            }
            if !state.send_in_flight
                && !state.edit_in_flight
                && state.message_id.is_none()
                && state.displayed_segment.is_none()
            {
                Some((Arc::clone(&state.http), state.channel_id.clone()))
            } else {
                None
            }
        };

        if let Some((http, channel_id)) = fallback {
            let parsed_channel_id = parse_channel_id(&channel_id)?;
            parsed_channel_id
                .say(&http, content)
                .await
                .map_err(|e: serenity::Error| FrameworkError::Tool(e.to_string()))?;
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
        spawn_next_streaming_display_action(display);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<Mutex<DiscordStreamingState>> {
        Arc::new(Mutex::new(DiscordStreamingState {
            http: Arc::new(Http::new("test-token")),
            channel_id: "123".to_owned(),
            channel_limit: 2_000,
            latest_content: String::new(),
            committed_prefix_chars: 0,
            displayed_segment: None,
            message_id: None,
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            edit_count: 0,
            initial_send_attempted: false,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: None,
            notify: Arc::new(Notify::new()),
            tool_status: None,
        }))
    }

    #[test]
    fn active_streaming_segment_reserves_room_for_tool_status_within_limit() {
        let segment = active_streaming_segment(&"a".repeat(1_995), 0, Some(2_000), Some("grep"))
            .expect("segment should exist");

        assert!(segment.content.ends_with("grep..._"));
        assert!(segment.content.chars().count() <= 2_000);
        assert!(segment.visible_chars < 1_995);
    }

    #[test]
    fn active_streaming_segment_suppresses_tool_status_when_suffix_exceeds_limit() {
        let long_status = "x".repeat(3_000);
        let segment = active_streaming_segment("hello", 0, Some(50), Some(&long_status))
            .expect("segment should exist");

        assert_eq!(segment.content, "hello");
        assert_eq!(segment.visible_chars, 5);
    }

    #[test]
    fn try_rollover_streaming_segment_resets_state_after_overflow_segment() {
        let mut state = DiscordStreamingState {
            http: Arc::new(Http::new("test-token")),
            channel_id: "123".to_owned(),
            channel_limit: 10,
            latest_content: "abcdefghijk".to_owned(),
            committed_prefix_chars: 0,
            displayed_segment: Some("abcdefghij".to_owned()),
            message_id: Some("msg-1".to_owned()),
            last_edit: Instant::now() - STREAMING_EDIT_INTERVAL,
            edit_count: 2,
            initial_send_attempted: true,
            send_in_flight: false,
            edit_in_flight: false,
            finalized: false,
            terminal_failure: false,
            error_message: Some("old error".to_owned()),
            notify: Arc::new(Notify::new()),
            tool_status: None,
        };

        assert!(try_rollover_streaming_segment(&mut state));
        assert_eq!(state.committed_prefix_chars, 10);
        assert!(state.displayed_segment.is_none());
        assert!(state.message_id.is_none());
        assert!(!state.initial_send_attempted);
        assert!(state.error_message.is_none());
        assert_eq!(state.edit_count, 0);
    }

    #[test]
    fn next_streaming_display_action_rate_limits_edits() {
        let display = test_state();
        {
            let mut state = display.lock().expect("state mutex should not be poisoned");
            state.latest_content = "fresh content".to_owned();
            state.displayed_segment = Some("stale content".to_owned());
            state.message_id = Some("msg-1".to_owned());
            state.initial_send_attempted = true;
            state.last_edit = Instant::now();
        }

        assert!(next_streaming_display_action(&display).is_none());
    }
}
