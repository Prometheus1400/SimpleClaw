use crate::config::{GatewayChannelKind, ToolCallTransparency};
use crate::dispatch::ToolExecutionResult;
use crate::telemetry::sanitize_preview;

pub(super) fn render_tool_call_transparency(
    reply: &str,
    tool_calls: &[ToolExecutionResult],
    mode: ToolCallTransparency,
    channel_kind: GatewayChannelKind,
) -> String {
    if mode == ToolCallTransparency::Off || tool_calls.is_empty() {
        return reply.to_owned();
    }

    let style = style_for_channel(channel_kind).unwrap_or(TransparencyRenderStyle::PlainFooter);
    let summary_lines = summary_lines(tool_calls, mode);
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

fn summary_lines(tool_calls: &[ToolExecutionResult], mode: ToolCallTransparency) -> Vec<String> {
    match mode {
        ToolCallTransparency::Off => Vec::new(),
        ToolCallTransparency::Concise => concise_summary_lines(tool_calls),
        ToolCallTransparency::Detailed => detailed_summary_lines(tool_calls),
    }
}

fn concise_summary_lines(tool_calls: &[ToolExecutionResult]) -> Vec<String> {
    let mut lines = Vec::new();
    for call in tool_calls {
        append_concise_tree(call, 0, &mut lines);
    }
    lines
}

fn detailed_summary_lines(tool_calls: &[ToolExecutionResult]) -> Vec<String> {
    let flattened = flatten_tool_calls(tool_calls);
    let mut lines = Vec::with_capacity(1 + flattened.len());
    lines.push("tool calls (detailed):".to_owned());
    for (idx, (depth, path, call)) in flattened.into_iter().enumerate() {
        let status = if call.success { "ok" } else { "error" };
        let args = sanitize_preview(&call.args_json, 120);
        let output = sanitize_preview(&call.output, 160);
        lines.push(format!(
            "{} | {} | {} | {}ms | path:{} | depth:{} | args:{} | out:{}",
            idx + 1,
            status,
            call.name,
            call.elapsed_ms,
            path,
            depth,
            args,
            output
        ));
    }
    lines
}

fn flatten_tool_calls(
    tool_calls: &[ToolExecutionResult],
) -> Vec<(usize, String, &ToolExecutionResult)> {
    let mut flattened = Vec::new();
    for (idx, call) in tool_calls.iter().enumerate() {
        let path = (idx + 1).to_string();
        append_flattened(call, 0, &path, &mut flattened);
    }
    flattened
}

fn append_flattened<'a>(
    call: &'a ToolExecutionResult,
    depth: usize,
    path: &str,
    out: &mut Vec<(usize, String, &'a ToolExecutionResult)>,
) {
    out.push((depth, path.to_owned(), call));
    for (idx, child) in call.nested_tool_calls.iter().enumerate() {
        let child_path = format!("{path}.{}", idx + 1);
        append_flattened(child, depth + 1, &child_path, out);
    }
}

fn append_concise_tree(call: &ToolExecutionResult, depth: usize, out: &mut Vec<String>) {
    let status = if call.success { "ok" } else { "error" };
    if depth == 0 {
        out.push(format!("({status}|{})", call.name));
    } else {
        let arrow = format!("{}>", "-".repeat(depth));
        out.push(format!("{arrow} ({status}|{})", call.name));
    }
    for child in &call.nested_tool_calls {
        append_concise_tree(child, depth + 1, out);
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
    use crate::config::{GatewayChannelKind, ToolCallTransparency};
    use crate::dispatch::ToolExecutionResult;

    #[test]
    fn off_mode_returns_reply_unchanged() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            ToolCallTransparency::Off,
            GatewayChannelKind::Discord,
        );
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn concise_mode_with_no_calls_returns_reply_unchanged() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn detailed_mode_with_no_calls_returns_reply_unchanged() {
        let rendered = render_tool_call_transparency(
            "base reply",
            &[],
            ToolCallTransparency::Detailed,
            GatewayChannelKind::Discord,
        );
        assert_eq!(rendered, "base reply");
    }

    #[test]
    fn concise_discord_uses_single_line_token_format() {
        let calls = vec![ToolExecutionResult {
            name: "clock".to_owned(),
            args_json: "{}".to_owned(),
            output: "2026-03-08T12:00:00Z".to_owned(),
            success: true,
            elapsed_ms: 4,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("\n-# (ok|clock)"));
    }

    #[test]
    fn concise_renders_multiple_top_level_calls_on_separate_lines() {
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
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# (ok|clock)\n-# (error|clock)"));
    }

    #[test]
    fn detailed_mode_redacts_sensitive_values_and_uses_row_layout() {
        let calls = vec![ToolExecutionResult {
            name: "web_fetch".to_owned(),
            args_json: r#"{"url":"https://example.com","api_key":"secret-123"}"#.to_owned(),
            output: r#"authorization: Bearer super-secret"#.to_owned(),
            success: false,
            elapsed_ms: 11,
            tool_call_id: None,
            nested_tool_calls: Vec::new(),
        }];
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Detailed,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# tool calls (detailed):"));
        assert!(rendered.contains("-# 1 | error | web\\_fetch | 11ms | path:1 | depth:0 | args:"));
        assert!(rendered.contains("| out:"));
        assert!(rendered.contains("***REDACTED***"));
        assert!(!rendered.contains("secret-123"));
        assert!(!rendered.contains("super-secret"));
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
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# (ok|web\\_fetch)"));
        assert!(!rendered.contains("-# (ok|web_fetch)"));
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
    fn nested_calls_are_included_in_concise_and_detailed() {
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

        let concise = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert!(concise.contains("-# (ok|summon)\n-# -> (ok|clock)"));

        let detailed = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Detailed,
            GatewayChannelKind::Discord,
        );
        assert!(detailed.contains("path:1 | depth:0"));
        assert!(detailed.contains("path:1.1 | depth:1"));
    }

    #[test]
    fn concise_uses_dash_depth_markers_for_deep_nesting() {
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
        let rendered = render_tool_call_transparency(
            "base reply",
            &calls,
            ToolCallTransparency::Concise,
            GatewayChannelKind::Discord,
        );
        assert!(rendered.contains("-# (ok|task)\n-# -> (ok|summon)\n-# --> (ok|exec)"));
    }
}
