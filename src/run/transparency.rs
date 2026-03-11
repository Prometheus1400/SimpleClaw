use std::collections::HashMap;

use crate::config::GatewayChannelKind;
use crate::dispatch::{ToolExecutionResult, ToolExecutionStatus};

pub(super) fn render_tool_call_transparency(
    reply: &str,
    tool_calls: &[ToolExecutionResult],
    tool_calls_enabled: bool,
    memory_recall_enabled: bool,
    memory_recall_used: bool,
    memory_recall_short_hits: usize,
    memory_recall_long_hits: usize,
    channel_kind: GatewayChannelKind,
) -> String {
    let summary = transparency_summary(
        tool_calls,
        tool_calls_enabled,
        memory_recall_enabled,
        memory_recall_used,
        memory_recall_short_hits,
        memory_recall_long_hits,
    );
    if summary.is_empty() {
        return reply.to_owned();
    }

    let style = style_for_channel(channel_kind).unwrap_or(TransparencyRenderStyle::PlainFooter);
    render_for_style(reply, &summary, style)
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TransparencySummary {
    tool_groups: Vec<ToolSummaryGroup>,
    memory_hits: Option<MemoryTransparencySummary>,
}

impl TransparencySummary {
    fn is_empty(&self) -> bool {
        self.tool_groups.is_empty() && self.memory_hits.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoryTransparencySummary {
    short_hits: usize,
    long_hits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolSummaryGroup {
    name: String,
    ok: usize,
    accepted: usize,
    err: usize,
}

fn transparency_summary(
    tool_calls: &[ToolExecutionResult],
    tool_calls_enabled: bool,
    memory_recall_enabled: bool,
    memory_recall_used: bool,
    memory_recall_short_hits: usize,
    memory_recall_long_hits: usize,
) -> TransparencySummary {
    let mut summary = TransparencySummary::default();
    if tool_calls_enabled && !tool_calls.is_empty() {
        summary.tool_groups = tool_summary_groups(tool_calls);
    }
    if memory_recall_enabled && memory_recall_used {
        summary.memory_hits = Some(MemoryTransparencySummary {
            short_hits: memory_recall_short_hits,
            long_hits: memory_recall_long_hits,
        });
    }
    summary
}

fn tool_summary_groups(tool_calls: &[ToolExecutionResult]) -> Vec<ToolSummaryGroup> {
    let mut order = Vec::new();
    let mut counts: HashMap<&str, (usize, usize, usize)> = HashMap::new();
    for call in flatten_tool_calls(tool_calls) {
        let entry = counts.entry(call.name.as_str()).or_insert_with(|| {
            order.push(call.name.as_str());
            (0, 0, 0)
        });
        match call.status {
            ToolExecutionStatus::Ok => entry.0 += 1,
            ToolExecutionStatus::Accepted => entry.1 += 1,
            ToolExecutionStatus::ToolError => entry.2 += 1,
        }
    }

    order
        .into_iter()
        .map(|name| {
            let (ok, accepted, err) = counts[name];
            ToolSummaryGroup {
                name: name.to_owned(),
                ok,
                accepted,
                err,
            }
        })
        .collect()
}

fn format_tool_summary_plain(groups: &[ToolSummaryGroup]) -> String {
    if groups.is_empty() {
        return "tools: [none]".to_owned();
    }
    let rendered = groups
        .iter()
        .map(|group| {
            let mut parts = Vec::new();
            if group.ok > 0 {
                parts.push(format!("ok×{}", group.ok));
            }
            if group.accepted > 0 {
                parts.push(format!("accepted×{}", group.accepted));
            }
            if group.err > 0 {
                parts.push(format!("err×{}", group.err));
            }
            format!("[{} {}]", group.name, parts.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("tools: {rendered}")
}

fn format_tool_summary_discord(groups: &[ToolSummaryGroup]) -> String {
    if groups.is_empty() {
        return "tools [none]".to_owned();
    }
    let rendered = groups
        .iter()
        .map(|group| {
            let mut parts = Vec::new();
            if group.ok > 0 {
                parts.push(format!("`ok×{}`", group.ok));
            }
            if group.accepted > 0 {
                parts.push(format!("`accepted×{}`", group.accepted));
            }
            if group.err > 0 {
                parts.push(format!("`err×{}`", group.err));
            }
            format!("[{} {}]", group.name, parts.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("tools {rendered}")
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
    summary: &TransparencySummary,
    style: TransparencyRenderStyle,
) -> String {
    if summary.is_empty() {
        return reply.to_owned();
    }

    match style {
        TransparencyRenderStyle::PlainFooter => {
            let mut parts = Vec::new();
            if !summary.tool_groups.is_empty() {
                parts.push(format_tool_summary_plain(&summary.tool_groups));
            }
            if let Some(hits) = summary.memory_hits {
                parts.push(format!(
                    "recall: short={} long={}",
                    hits.short_hits, hits.long_hits
                ));
            }
            format!("{reply}\n---\n{}", parts.join(" | "))
        }
        TransparencyRenderStyle::DiscordSubtext => {
            let mut parts = Vec::new();
            if !summary.tool_groups.is_empty() {
                parts.push(format_tool_summary_discord(&summary.tool_groups));
            }
            if let Some(hits) = summary.memory_hits {
                parts.push(format!(
                    "recall `short={} long={}`",
                    hits.short_hits, hits.long_hits
                ));
            }
            let subtext = format!("-# {}", escape_discord_subtext(&parts.join(" • ")));
            format!("{reply}\n{subtext}")
        }
    }
}

fn escape_discord_subtext(line: &str) -> String {
    line.replace('_', r"\_")
}

#[cfg(test)]
mod tests {
    use super::{
        ToolSummaryGroup, TransparencyRenderStyle, TransparencySummary, render_for_style,
        render_tool_call_transparency,
    };
    use crate::config::GatewayChannelKind;
    use crate::dispatch::{ToolExecutionResult, ToolExecutionStatus};

    fn render(reply: &str, calls: &[ToolExecutionResult], tool_calls_enabled: bool) -> String {
        render_tool_call_transparency(
            reply,
            calls,
            tool_calls_enabled,
            false,
            false,
            0,
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
            status: ToolExecutionStatus::Ok,
            elapsed_ms: 4,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("\n-# tools [clock `ok×1`]"));
        assert!(!rendered.contains("okx1"));
    }

    #[test]
    fn repeated_calls_are_aggregated_with_ok_and_err_counts() {
        let calls = vec![
            ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"UTC"}"#.to_owned(),
                output: "2026-03-08T12:00:00Z".to_owned(),
                status: ToolExecutionStatus::Ok,
                elapsed_ms: 4,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            },
            ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: r#"{"timezone":"Invalid/Zone"}"#.to_owned(),
                output: "tool_error: invalid timezone".to_owned(),
                status: ToolExecutionStatus::ToolError,
                elapsed_ms: 9,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            },
        ];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools [clock `ok×1` `err×1`]"));
        assert!(!rendered.contains("okx1"));
        assert!(!rendered.contains("errx1"));
    }

    #[test]
    fn grouped_summary_preserves_first_seen_tool_order() {
        let calls = vec![ToolExecutionResult {
            name: "web_fetch".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            status: ToolExecutionStatus::ToolError,
            elapsed_ms: 11,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: "{}".to_owned(),
                output: "ok".to_owned(),
                status: ToolExecutionStatus::Ok,
                elapsed_ms: 1,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools [web\\_fetch `err×1`] [clock `ok×1`]"));
    }

    #[test]
    fn discord_subtext_escapes_underscores() {
        let calls = vec![ToolExecutionResult {
            name: "web_fetch".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            status: ToolExecutionStatus::Ok,
            elapsed_ms: 1,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools [web\\_fetch `ok×1`]"));
        assert!(!rendered.contains("-# tools [web_fetch `ok×1`]"));
    }

    #[test]
    fn plain_footer_style_is_available_for_channel_fallbacks() {
        let summary = TransparencySummary {
            tool_groups: vec![ToolSummaryGroup {
                name: "clock".to_owned(),
                ok: 1,
                accepted: 0,
                err: 0,
            }],
            memory_hits: None,
        };
        let rendered =
            render_for_style("base reply", &summary, TransparencyRenderStyle::PlainFooter);
        assert_eq!(rendered, "base reply\n---\ntools: [clock ok×1]");
    }

    #[test]
    fn nested_calls_are_included_in_grouped_summary() {
        let calls = vec![ToolExecutionResult {
            name: "summon".to_owned(),
            args_json: r#"{"agent":"research"}"#.to_owned(),
            output: "delegated".to_owned(),
            status: ToolExecutionStatus::Ok,
            elapsed_ms: 14,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "clock".to_owned(),
                args_json: "{}".to_owned(),
                output: "2026-03-08T12:00:00Z".to_owned(),
                status: ToolExecutionStatus::Ok,
                elapsed_ms: 3,
                tool_call_id: None,
                nested_tool_calls: Vec::new(),
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools [summon `ok×1`] [clock `ok×1`]"));
    }

    #[test]
    fn deep_nesting_is_counted_in_flattened_grouping() {
        let calls = vec![ToolExecutionResult {
            name: "task".to_owned(),
            args_json: "{}".to_owned(),
            output: "ok".to_owned(),
            status: ToolExecutionStatus::Ok,
            elapsed_ms: 10,
            tool_call_id: None,
            nested_tool_calls: vec![ToolExecutionResult {
                name: "summon".to_owned(),
                args_json: "{}".to_owned(),
                output: "ok".to_owned(),
                status: ToolExecutionStatus::Ok,
                elapsed_ms: 9,
                tool_call_id: None,
                nested_tool_calls: vec![ToolExecutionResult {
                    name: "exec".to_owned(),
                    args_json: "{}".to_owned(),
                    output: "ok".to_owned(),
                    status: ToolExecutionStatus::Ok,
                    elapsed_ms: 7,
                    tool_call_id: None,
                    nested_tool_calls: Vec::new(),
                }],
            }],
        }];
        let rendered = render("base reply", &calls, true);
        assert!(rendered.contains("-# tools [task `ok×1`] [summon `ok×1`] [exec `ok×1`]"));
    }

    #[test]
    fn memory_recall_transparency_renders_used_with_hits() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            false,
            true,
            true,
            0,
            2,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# recall `short=0 long=2`"));
    }

    #[test]
    fn memory_recall_transparency_renders_short_and_long_hits() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            false,
            true,
            true,
            1,
            2,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# recall `short=1 long=2`"));
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
            status: ToolExecutionStatus::Ok,
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
            1,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# tools [clock `ok×1`] • recall `short=1 long=1`"));
        assert!(!rendered.contains("\n-# memory"));
    }
}
