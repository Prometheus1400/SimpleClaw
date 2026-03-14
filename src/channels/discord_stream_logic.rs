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
) -> Option<ActiveStreamingSegment> {
    let start = byte_index_for_char_offset(latest_content, committed_prefix_chars);
    let tail = &latest_content[start..];
    if tail.is_empty() {
        return None;
    }

    let tail_chars = tail.chars().count();
    let visible_chars = channel_limit.map_or(tail_chars, |limit| limit.min(tail_chars));
    let end = byte_index_for_char_offset(tail, visible_chars);
    Some(ActiveStreamingSegment {
        content: tail[..end].to_owned(),
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
    true
}

enum DiscordStreamingStateAction {
    SendInitial {
        gateway: Arc<crate::gateway::Gateway>,
        inbound: InboundMessage,
        content: String,
    },
    Edit {
        gateway: Arc<crate::gateway::Gateway>,
        inbound: InboundMessage,
        message_id: String,
        content: String,
    },
}

fn spawn_streaming_display_update(display: &Arc<Mutex<DiscordStreamingState>>, content: &str) {
    {
        let mut state = match display.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.latest_content = content.to_owned();
        state.notify.notify_waiters();
    }
    spawn_next_streaming_display_action(display);
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
    )?;

    if state.message_id.is_none() {
        if state.send_in_flight || state.displayed_segment.is_some() || state.initial_send_attempted
        {
            return None;
        }
        state.initial_send_attempted = true;
        state.send_in_flight = true;
        return Some(DiscordStreamingStateAction::SendInitial {
            gateway: Arc::clone(&state.http),
            inbound: state.channel_id.clone(),
            content: segment.content,
        });
    }

    if state.edit_in_flight {
        return None;
    }

    if state.displayed_segment.as_deref() == Some(segment.content.as_str()) {
        return None;
    }

    if !state.finalized && state.last_edit.elapsed() < STREAMING_EDIT_INTERVAL {
        return None;
    }

    let message_id = state.message_id.clone()?;
    state.edit_in_flight = true;
    Some(DiscordStreamingStateAction::Edit {
        gateway: Arc::clone(&state.http),
        inbound: state.channel_id.clone(),
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
                gateway,
                inbound,
                content,
            } => {
                let result = gateway.send_message_with_id(&inbound, &content).await;
                let mut should_retry = false;
                {
                    let mut state = match display.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    state.send_in_flight = false;
                    match result {
                        Ok(Some(message_id)) => {
                            state.message_id = Some(message_id);
                            state.displayed_segment = Some(content);
                            state.last_edit = Instant::now();
                            state.error_message = None;
                            should_retry = true;
                        }
                        Ok(None) => {
                            state.displayed_segment = Some(content);
                            state.error_message = None;
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
                gateway,
                inbound,
                message_id,
                content,
            } => {
                let result = gateway.edit_message(&inbound, &message_id, &content).await;
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
                )
                .map(|segment| state.displayed_segment.as_deref() == Some(segment.content.as_str()))
                .unwrap_or(false)
                && state.committed_prefix_chars
                    + active_streaming_segment(
                        &state.latest_content,
                        state.committed_prefix_chars,
                        Some(state.channel_limit),
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
            parsed_channel_id.say(&http, content).await.map_err(|e| FrameworkError::Tool(e.to_string()))?;
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
        spawn_next_streaming_display_action(display);
    }
}
