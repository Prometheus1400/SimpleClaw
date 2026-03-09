use std::collections::HashMap;

use crate::config::GatewayChannelKind;
use crate::dispatch::ToolExecutionResult;

pub(super) fn render_tool_call_transparency(
    reply: &str,
    tool_calls: &[ToolExecutionResult],
    tool_calls_enabled: bool,
    memory_recall_enabled: bool,
    memory_recall_used: bool,
    memory_recall_hits: usize,
    channel_kind: GatewayChannelKind,
) -> String {
    let summary_lines = summary_lines(
        tool_calls,
        tool_calls_enabled,
        memory_recall_enabled,
        memory_recall_used,
        memory_recall_hits,
    );
    if summary_lines.is_empty() {
        return reply.to_owned();
    }

    let style = style_for_channel(channel_kind).unwrap_or(TransparencyRenderStyle::PlainFooter);
    render_for_style(reply, &summary_lines, style)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransparencyRenderStyle {
    PlainFooter,
    DiscordSubtext,
}

fn style_for_channel(kind: GatewayChannelKind) -> Option<TransparencyRenderStyle> {
    match kind {
        GatewayChannelKind::Discord => Some(TransparencyRenderStyle::DiscordSubtext),
    }
}

fn summary_lines(
    tool_calls: &[ToolExecutionResult],
    tool_calls_enabled: bool,
    memory_recall_enabled: bool,
    memory_recall_used: bool,
    memory_recall_hits: usize,
) -> Vec<String> {
    let mut parts = Vec::new();
    if tool_calls_enabled && !tool_calls.is_empty() {
        parts.push(tool_summary_line(tool_calls));
    }
    if memory_recall_enabled && memory_recall_used {
        parts.push(memory_recall_summary_line(memory_recall_hits));
    }
    if parts.is_empty() {
        Vec::new()
    } else {
        vec![parts.join(" | ")]
    }
}

fn tool_summary_line(tool_calls: &[ToolExecutionResult]) -> String {
    let mut order = Vec::new();
    let mut counts: HashMap<&str, (usize, usize)> = HashMap::new();
    for call in flatten_tool_calls(tool_calls) {
        let entry = counts.entry(call.name.as_str()).or_insert_with(|| {
            order.push(call.name.as_str());
            (0, 0)
        });
        if call.success {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }

    let groups = order
        .into_iter()
        .map(|name| {
            let (ok, err) = counts[name];
            let mut parts = Vec::new();
            if ok > 0 {
                parts.push(format!("ok×{ok}"));
            }
            if err > 0 {
                parts.push(format!("err×{err}"));
            }
            format!("[{name} {}]", parts.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ");
    if groups.is_empty() {
        "tools: [none]".to_owned()
    } else {
        format!("tools: {groups}")
    }
}

fn memory_recall_summary_line(hits: usize) -> String {
    format!("memory: hits={hits}")
}

fn flatten_tool_calls(tool_calls: &[ToolExecutionResult]) -> Vec<&ToolExecutionResult> {
    let mut flattened = Vec::new();
    for call in tool_calls {
        append_flattened(call, &mut flattened);
    }
    flattened
}

fn append_flattened<'a>(call: &'a ToolExecutionResult, out: &mut Vec<&'a ToolExecutionResult>) {
    out.push(call);
    for child in &call.nested_tool_calls {
        append_flattened(child, out);
    }
}

fn render_for_style(
    reply: &str,
    summary_lines: &[String],
    style: TransparencyRenderStyle,
) -> String {
    if summary_lines.is_empty() {
        return reply.to_owned();
    }

    match style {
        TransparencyRenderStyle::PlainFooter => {
            format!("{reply}\n---\n{}", summary_lines.join("\n"))
        }
        TransparencyRenderStyle::DiscordSubtext => {
            let subtext = summary_lines
                .iter()
                .map(|line| format!("-# {}", escape_discord_subtext(line)))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{reply}\n{subtext}")
        }
    }
}

fn escape_discord_subtext(line: &str) -> String {
    line.replace('_', r"\_")
}

#[cfg(test)]
mod tests {
    use super::{TransparencyRenderStyle, render_for_style, render_tool_call_transparency};
    use crate::config::GatewayChannelKind;
    use crate::dispatch::ToolExecutionResult;

    fn render(reply: &str, calls: &[ToolExecutionResult], tool_calls_enabled: bool) -> String {
        render_tool_call_transparency(
            reply,
            calls,
            tool_calls_enabled,
            false,
            false,
            0,
            GatewayChannelKind::Discord,
        )
    }

    #[test]
    fn off_mode_returns_reply_unchanged() {
        let rendered = render("base reply", &[], false);
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn enabled_mode_with_no_calls_returns_reply_unchanged() {
        let rendered = render("base reply", &[], true);
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn enabled_mode_renders_grouped_status_chips() {
        let calls = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            args_json: "{}".to_owned(),
            output: "2026-03-08T12:00:00Z".to_owned(),
            success: true,
            elapsed_ms: 4,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("\n-# tools: [clock ok×1]"));
        assert!(!rendered.contains("okx1"));
    }

    #[test]
    fn repeated_calls_are_aggregated_with_ok_and_err_counts() {
        let calls = vec![
            ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                output: "2026-03-08T12:00:00Z".to_owned(),
                success: true,
                elapsed_ms: 4,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            },
            ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"Invalid/Zone"}"#.to_owned(),
                output: "tool_error: invalid timezone".to_owned(),
                success: false,
                elapsed_ms: 9,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            },
        ];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools: [clock ok×1 err×1]"));
        assert!(!rendered.contains("okx1"));
        assert!(!rendered.contains("errx1"));
    }

    #[test]
    fn grouped_summary_preserves_first_seen_tool_order() {
        let calls = vec![ToolExecutionResult {
            name: "web_fetch".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            success: false,
            elapsed_ms: 11,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: "{}".to_owned(),
                output: "ok".to_owned(),
                success: true,
                elapsed_ms: 1,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools: [web\\_fetch err×1] [clock ok×1]"));
    }

    #[test]
    fn discord_subtext_escapes_underscores() {
        let calls = vec![ToolExecutionResult {
            name: "web_fetch".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            success: true,
            elapsed_ms: 1,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools: [web\\_fetch ok×1]"));
        assert!(!rendered.contains("-# tools: [web_fetch ok×1]"));
    }

    #[test]
    fn plain_footer_style_is_available_for_channel_fallbacks() {
        let rendered = render_for_style(
            "base reply",
            &["tool calls: 1".to_owned()],
            TransparencyRenderStyle::PlainFooter,
        );
        assert_eq!(rendered, "base reply\n---\ntool calls: 1");
    }

    #[test]
    fn nested_calls_are_included_in_grouped_summary() {
        let calls = vec![ToolExecutionResult {
            name: "summon".to_owned(),
            args_json: r#"{"agent":"research"}"#.to_owned(),
            output: "delegated".to_owned(),
            success: true,
            elapsed_ms: 14,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: "{}".to_owned(),
                output: "2026-03-08T12:00:00Z".to_owned(),
                success: true,
                elapsed_ms: 3,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools: [summon ok×1] [clock ok×1]"));
    }

    #[test]
    fn deep_nesting_is_counted_in_flattened_grouping() {
        let calls = vec![ToolExecutionResult {
            name: "task".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            success: true,
            elapsed_ms: 10,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "summon".to_owned(),
                args_json: "{}".to_owned(),
                output: "ok".to_owned(),
                success: true,
                elapsed_ms: 9,
                tool_call_id: None,
                nested_tool_calls: vec![ToolExecutionResult {
                    name: "exec".to_owned(),
                    args_json: "{}".to_owned(),
                    output: "ok".to_owned(),
                    success: true,
                    elapsed_ms: 7,
                    tool_call_id: None,
                    nested_tool_calls: Vec::new(),
                }],
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools: [task ok×1] [summon ok×1] [exec ok×1]"));
    }

    #[test]
    fn memory_recall_transparency_renders_used_with_hits() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            false,
            true,
            true,
            2,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# memory: hits=2"));
    }

    #[test]
    fn memory_recall_transparency_omits_line_when_not_used() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            false,
            true,
            false,
            0,
            GatewayChannelKind::Discord,
        );
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn memory_recall_line_can_render_with_tool_summary() {
        let calls = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            success: true,
            elapsed_ms: 1,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            true,
            true,
            true,
            1,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# tools: [clock ok×1] | memory: hits=1"));
        assert!(!rendered.contains("\n-# memory"));
    }
}
