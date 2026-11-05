use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::app::{ChatLine, LineType, highlight_code_block, pad_to_width, wrap_text};
use crate::theme;

// ════════════════════════════════════════════════════════════════
// Helper functions (moved from app.rs)
// ════════════════════════════════════════════════════════════════

/// 工具名称 → emoji 图标映射
pub fn tool_icon(name: &str) -> &'static str {
    match name {
        "read_file" | "read_files" => "📖",
        "write_file" => "📝",
        "edit_file" => "✏️",
        "grep" => "🔍",
        "glob" => "📂",
        "bash" | "cmd" | "powershell" => "💻",
        "fetch_web" | "fetch" => "🌐",
        n if n.starts_with("codegraph_") => "🔎",
        _ => "🔧",
    }
}

/// 根据工具名获取结果最大行数
/// - None = 不截断
/// - Some(0) = 不显示内容
/// - Some(N) = 最多 N 行
pub fn max_lines_for_tool(name: &str) -> Option<usize> {
    match name {
        "read_file" | "read_files" => Some(0),
        "edit_file" | "write_file" => None,
        "bash" | "cmd" | "powershell" => Some(30),
        "grep" => Some(20),
        "glob" => Some(15),
        "fetch_web" | "fetch" => Some(20),
        n if n.starts_with("codegraph_") => Some(20),
        _ => Some(20),
    }
}

/// 统计 unified diff 中的删除行数和新增行数。
/// diff 内容中，以 `-` 开头的行是删除行（排除 `---` header），以 `+` 开头的行是新增行（排除 `+++` header）。
fn count_diff_lines(content: &str) -> (usize, usize) {
    let mut removed = 0usize;
    let mut added = 0usize;
    for line in content.lines() {
        if line.starts_with("---") {
            continue;
        }
        if line.starts_with("+++") {
            continue;
        }
        if line.starts_with('-') {
            removed += 1;
        } else if line.starts_with('+') {
            added += 1;
        }
    }
    (removed, added)
}

/// read_file 类工具的摘要行
pub fn result_summary(name: &str, content: &str) -> String {
    match name {
        "read_file" | "read_files" => {
            let lines = content.lines().count();
            let bytes = content.len();
            format!("Read {} bytes ({} lines)", bytes, lines)
        }
        "write_file" => {
            // WriteFile 成功结果格式: "Written N bytes to PATH\n<content>"
            // 闭合态摘要：显示写入的字节数和行数
            if let Some(first_line) = content.lines().next() {
                // 尝试从 "Written N bytes to PATH" 提取字节数
                if let Some(bytes) = first_line
                    .strip_prefix("Written ")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    let line_count = content.lines().count().saturating_sub(1); // 减去首行
                    format!("Written {} bytes ({} lines)", bytes, line_count)
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        }
        "edit_file" => {
            // EditFile 成功结果格式: "Replaced 1 occurrence in PATH\n<unified diff>"
            // 闭合态摘要：xx lines remove, xx lines add
            if content.starts_with("Replaced") {
                let (removed, added) = count_diff_lines(content);
                format!("{} lines remove, {} lines add", removed, added)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Format tool arguments as a concise summary string for display in the collapsible header.
pub(crate) fn format_tool_args_summary(tool_name: &str, args_json: &str) -> String {
    match tool_name {
        "read_file" | "read_files" => {
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .or_else(|| args.get("paths").and_then(|v| v.as_str()))
                    .unwrap_or("?");
                let start = args.get("start_line").and_then(|v| v.as_i64());
                let end = args.get("end_line").and_then(|v| v.as_i64());
                match (start, end) {
                    (Some(s), Some(e)) => format!("{}:{}-{}", path, s, e),
                    (Some(s), None) => format!("{}:{}-", path, s),
                    (None, Some(e)) => format!("{}:-{}", path, e),
                    (None, None) => path.to_string(),
                }
            } else {
                args_json.to_string()
            }
        }
        "write_file" => {
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let lines = content.lines().count();
                format!("{}: {}", path, lines)
            } else {
                args_json.to_string()
            }
        }
        "edit_file" => {
            // EditFile 参数: path/old_string/new_string
            // 闭合态 header: file_name (remove/add 行数从结果 diff 中统计)
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
                args.get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string()
            } else {
                args_json.to_string()
            }
        }
        "bash" | "cmd" | "powershell" => {
            // Bash 闭合态 header: command
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                // 截断过长的命令
                let chars: Vec<char> = cmd.chars().collect();
                if chars.len() > 60 {
                    let truncated: String = chars.into_iter().take(57).collect();
                    format!("{}...", truncated)
                } else {
                    cmd.to_string()
                }
            } else {
                args_json.to_string()
            }
        }
        _ => {
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
                if let Some(obj) = args.as_object() {
                    let parts: Vec<String> = obj
                        .iter()
                        .map(|(k, v)| {
                            let val_str = match v {
                                serde_json::Value::String(s) => s.clone(),
                                _ => v.to_string(),
                            };
                            let char_count = val_str.chars().count();
                            if char_count > 40 {
                                let truncated: String = val_str.chars().take(37).collect();
                                format!("{}:{}...", k, truncated)
                            } else {
                                format!("{}:{}", k, val_str)
                            }
                        })
                        .collect();
                    parts.join(" ")
                } else {
                    args_json.to_string()
                }
            } else {
                args_json.to_string()
            }
        }
    }
}

/// Format tool arguments as full human-readable text for the expanded view.
/// 不截断任何内容；JSON 解析失败时原样返回 args_json。
pub(crate) fn format_tool_args_full(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return args_json.to_string(),
    };

    match tool_name {
        "bash" | "cmd" | "powershell" => {
            // 完整 command，不截断
            args.get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        "read_file" | "read_files" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .or_else(|| args.get("paths").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let start = args.get("start_line").and_then(|v| v.as_i64());
            let end = args.get("end_line").and_then(|v| v.as_i64());
            match (start, end) {
                (Some(s), Some(e)) => format!("{}:{}-{}", path, s, e),
                (Some(s), None) => format!("{}:{}-", path, s),
                (None, Some(e)) => format!("{}:-{}", path, e),
                (None, None) => path.to_string(),
            }
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            format!("path: {}\n{}", path, content)
        }
        "edit_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let old_string = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_string = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!(
                "path: {}\n--- old\n{}\n+++ new\n{}",
                path, old_string, new_string
            )
        }
        _ => {
            if let Some(obj) = args.as_object() {
                let mut parts = Vec::new();
                for (k, v) in obj {
                    let val_str = match v {
                        serde_json::Value::String(s) => s.clone(),
                        _ => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                    };
                    parts.push(format!("{}: {}", k, val_str));
                }
                parts.join("\n")
            } else {
                serde_json::to_string_pretty(&args).unwrap_or_else(|_| args_json.to_string())
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════
// Tool rendering functions (extracted from MessageCache::from_message)
// ════════════════════════════════════════════════════════════════

/// Render a ToolCall message block.
pub(crate) fn render_tool_call(msg: &ChatLine, width: u16, expanded: bool) -> Vec<Line<'static>> {
    let name = match &msg.line_type {
        LineType::ToolCall { name } => name.as_str(),
        _ => return Vec::new(),
    };

    let icon = tool_icon(name);
    let mut lines = Vec::new();

    // Parse arguments for summary header
    let args_summary = format_tool_args_summary(name, &msg.content);
    let header = format!("{} {} {}", icon, name, args_summary);
    let header_wrapped = wrap_text(&header, width);

    for dl in header_wrapped.iter() {
        let content = if dl.is_empty() {
            " ".repeat(width as usize)
        } else {
            pad_to_width(dl, width as usize)
        };
        lines.push(Line::styled(
            content,
            Style::default().fg(theme::TOOL_CALL_FG),
        ));
    }

    // Expanded: show full tool arguments (before result, or standalone if no result yet)
    if expanded {
        let args_full = format_tool_args_full(name, &msg.content);
        if !args_full.is_empty() {
            // Empty separator line
            lines.push(Line::styled(
                pad_to_width("", width as usize),
                Style::default().fg(theme::TOOL_CALL_FG),
            ));
            // Full args, wrapped to width, no truncation
            let args_wrapped = wrap_text(&args_full, width);
            for dl in args_wrapped.iter() {
                let content = if dl.is_empty() {
                    " ".repeat(width as usize)
                } else {
                    pad_to_width(dl, width as usize)
                };
                lines.push(Line::styled(
                    content,
                    Style::default().fg(theme::TOOL_CALL_FG),
                ));
            }
        }
    }

    // If we have a merged result, show it
    if let Some(ref result) = msg.tool_result {
        if expanded {
            // Expanded: show full result content (separator already added with args block)
            let result_style = if msg.tool_error {
                Style::default().fg(theme::ERROR_FG)
            } else {
                Style::default().fg(theme::TOOL_RESULT_FG)
            };

            // Render result content with syntax highlighting
            let mut highlighted = highlight_code_block("", result);
            // Pad each line to full width
            for line in &mut highlighted {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let padded = if text.is_empty() {
                    " ".repeat(width as usize)
                } else {
                    pad_to_width(&text, width as usize)
                };
                // 保留 syntect 样式，只替换内容
                line.spans.clear();
                line.spans.push(Span::styled(padded, result_style));
            }
            // 展开态：完整显示结果内容，不截断
            lines.extend(highlighted);
        } else {
            // Collapsed: show result summary
            let summary = result_summary(name, result);
            if !summary.is_empty() {
                let summary_line = format!("  {}", summary);
                let summary_wrapped = wrap_text(&summary_line, width);
                for sl in &summary_wrapped {
                    let content = if sl.is_empty() {
                        " ".repeat(width as usize)
                    } else {
                        pad_to_width(sl, width as usize)
                    };
                    lines.push(Line::styled(
                        content,
                        Style::default().fg(theme::TOOL_RESULT_FG),
                    ));
                }
            }
        }
    }

    lines
}

/// Render an AgentCall message block (sub-agent invocation in main tab).
///
/// Layout:
/// ```text
/// 闭合:  agent_name: sub_session_id     [show in new tab]
///         agent prompt第一行
///
/// 展开:  agent_name: sub_session_id     [show in new tab]
///         agent 完整 prompt
///         agent result
/// ```
pub(crate) fn render_agent_call(msg: &ChatLine, width: u16, expanded: bool) -> Vec<Line<'static>> {
    let name = match &msg.line_type {
        LineType::AgentCall { name } => name.as_str(),
        _ => return Vec::new(),
    };

    let mut lines = Vec::new();

    // Parse prompt from arguments JSON
    let prompt = serde_json::from_str::<serde_json::Value>(&msg.content)
        .ok()
        .and_then(|v| v.get("prompt").and_then(|p| p.as_str()).map(String::from))
        .unwrap_or_else(|| msg.content.clone());

    // Short session ID (first 8 chars)
    let short_sid = msg
        .sub_session_id
        .as_deref()
        .map(|s| &s[..s.len().min(8)])
        .unwrap_or("...");

    // Header line: "agent_name: sub_session_id"  right-aligned "[show in new tab]"
    let left = format!("🤖 {}: {}", name, short_sid);
    let button = "[show in new tab]";
    let header_line = build_header_with_button(&left, button, width as usize);
    lines.push(Line::styled(
        header_line,
        Style::default().fg(theme::AGENT_CALL_FG),
    ));

    // Prompt line(s)
    if expanded {
        // Show full prompt
        let prompt_wrapped = wrap_text(&prompt, width);
        for dl in prompt_wrapped.iter() {
            let content = if dl.is_empty() {
                " ".repeat(width as usize)
            } else {
                pad_to_width(dl, width as usize)
            };
            lines.push(Line::styled(
                content,
                Style::default().fg(theme::TOOL_RESULT_FG),
            ));
        }

        // If we have a merged result, show it
        if let Some(ref result) = msg.tool_result {
            lines.push(Line::styled(
                pad_to_width("", width as usize),
                Style::default().fg(theme::TOOL_RESULT_FG),
            ));
            let result_style = if msg.tool_error {
                Style::default().fg(theme::ERROR_FG)
            } else {
                Style::default().fg(theme::TOOL_RESULT_FG)
            };
            let mut highlighted = highlight_code_block("", result);
            for line in &mut highlighted {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let padded = if text.is_empty() {
                    " ".repeat(width as usize)
                } else {
                    pad_to_width(&text, width as usize)
                };
                *line = Line::styled(padded, result_style);
            }
            lines.extend(highlighted);
        }
    } else {
        // Collapsed: show only first line of prompt
        let first_line = prompt.lines().next().unwrap_or("");
        let display = if first_line.is_empty() {
            " ".repeat(width as usize)
        } else {
            pad_to_width(first_line, width as usize)
        };
        lines.push(Line::styled(
            display,
            Style::default().fg(theme::TOOL_RESULT_FG),
        ));

        // If we have a result, show summary
        if let Some(ref result) = msg.tool_result {
            let summary = result.lines().next().unwrap_or("");
            if !summary.is_empty() {
                let summary_line = format!("  ✓ {}", summary);
                lines.push(Line::styled(
                    pad_to_width(&summary_line, width as usize),
                    Style::default().fg(theme::TOOL_RESULT_FG),
                ));
            }
        }
    }

    lines
}

/// Build a header line with left text and a right-aligned button.
/// The button is placed near the right edge with a small gap.
/// Uses display width (not char count) for correct alignment with CJK/emoji.
fn build_header_with_button(left: &str, button: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let left_len = left.width();
    let button_len = button.width();
    // 留 2 格右间距，按钮不贴到最右边
    let right_gap = 2;
    if left_len + button_len + right_gap + 1 >= width {
        // Not enough space: just show left text, truncated
        let truncated: String = left.chars().take(width).collect();
        return pad_to_width(&truncated, width);
    }
    let gap = width - right_gap - left_len - button_len;
    format!("{}{}{}", left, " ".repeat(gap), button)
}

/// Render a ToolResult message block.
pub(crate) fn render_tool_result(msg: &ChatLine, width: u16) -> Vec<Line<'static>> {
    let name = match &msg.line_type {
        LineType::ToolResult { name } => name.as_str(),
        _ => return Vec::new(),
    };

    let max_lines = max_lines_for_tool(name);
    let icon = tool_icon(name);
    let mut lines = Vec::new();
    if max_lines == Some(0) {
        // 不显示内容，只显示摘要
        let summary = result_summary(name, &msg.content);
        let status_line = format!("  ✓ {} {} {}", icon, name, summary);
        lines.push(Line::styled(
            pad_to_width(&status_line, width as usize),
            Style::default().fg(theme::TOOL_RESULT_FG),
        ));
    } else {
        // bash/shell 输出：首行就是内容，不走"灰色状态头"模式
        let is_shell = matches!(name, "bash" | "cmd" | "powershell");

        let content_body = if is_shell {
            // shell：全部内容传语法高亮，不加灰色头
            &msg.content
        } else if let Some((first, rest)) = msg.content.split_once('\n') {
            // 其他工具：第一行作为灰色状态头
            let status = format!("  ✓ {} {} {}", icon, name, first);
            lines.push(Line::styled(
                pad_to_width(&status, width as usize),
                Style::default().fg(theme::TOOL_RESULT_FG),
            ));
            rest
        } else {
            let summary = msg.content.clone();
            let status = format!("  ✓ {} {}", icon, summary);
            lines.push(Line::styled(
                pad_to_width(&status, width as usize),
                Style::default().fg(theme::TOOL_RESULT_FG),
            ));
            return lines;
        };

        let mut highlighted = highlight_code_block("", content_body);
        let total_len = highlighted.len();
        if let Some(max) = max_lines
            && total_len > max
        {
            highlighted.truncate(max);
            let remaining = total_len.saturating_sub(max);
            highlighted.push(Line::styled(
                format!("... [truncated, {} more lines]", remaining),
                Style::default().fg(theme::TOOL_RESULT_FG),
            ));
        }
        lines.extend(highlighted);
    }
    lines
}

/// Render a ToolError message block.
pub(crate) fn render_tool_error(msg: &ChatLine, width: u16) -> Vec<Line<'static>> {
    let name = match &msg.line_type {
        LineType::ToolError { name } => name.as_str(),
        _ => return Vec::new(),
    };

    let icon = tool_icon(name);
    let mut lines = Vec::new();
    let error_line = format!("❌ {} {} {}", icon, name, msg.content);
    lines.push(Line::styled(
        pad_to_width(&error_line, width as usize),
        Style::default().fg(theme::ERROR_FG),
    ));
    lines
}

// ════════════════════════════════════════════════════════════════
// Click hit-test (extracted from event.rs)
// ════════════════════════════════════════════════════════════════

/// Check if a mouse click at the given virtual_row hits the clickable header region of a
/// tool/agent call block. If so, returns the call_id to toggle. Returns None otherwise.
///
/// 可点击区域（仅 block 内容区的前两行，见 ui.md 设计）：
/// - ToolCall：header 摘要行（过长换行时最多前两行）
/// - AgentCall：agent_name 行 + agent prompt 第一行
///
/// 其余区域（args、tool results 等）不触发折叠切换，放行给文本选择。
pub(crate) fn tool_block_hit_test(
    messages: &[ChatLine],
    caches: &[crate::app::MessageCache],
    virtual_row: u16,
    width: u16,
) -> Option<String> {
    let mut y: u16 = 0;
    for msg in messages {
        let style = theme::style_for(msg.line_type.clone());
        if let Some(cache) = caches.iter().find(|c| c.msg_id == msg.id) {
            let h = style.total_height(cache.line_count);
            if virtual_row >= y && virtual_row < y + h {
                // Clicked on this message
                if let Some(ref call_id) = msg.call_id
                    && matches!(
                        msg.line_type,
                        LineType::ToolCall { .. } | LineType::AgentCall { .. }
                    )
                {
                    // 仅 header 区域可点击切换展开/折叠
                    let start = y + style.top_margin + style.margin_vertical;
                    let rows = clickable_header_rows(msg, width, h);
                    if virtual_row >= start && virtual_row < start.saturating_add(rows) {
                        return Some(call_id.clone());
                    }
                }
                break;
            }
            y += h;
        }
    }
    None
}

/// 计算 tool/agent block 中可触发折叠切换的 header 行数。
///
/// - ToolCall：header 摘要行（icon + name + args summary）
/// - AgentCall：header 行（agent_name: sid [button]）+ agent prompt 第一行
///
/// 无论折叠/展开，可点击行数一律封顶为 2（"前两行"）：展开态下的
/// 完整 args、tool results 等区域留给文本选择，点击不再触发折叠。
///
/// `block_h` 为整个 block 的渲染高度，用于兜底防止越界。
fn clickable_header_rows(msg: &ChatLine, width: u16, block_h: u16) -> u16 {
    let rows = match &msg.line_type {
        LineType::ToolCall { name } => {
            let icon = tool_icon(name);
            let args_summary = format_tool_args_summary(name, &msg.content);
            let header = format!("{} {} {}", icon, name, args_summary);
            wrap_text(&header, width).len() as u16
        }
        LineType::AgentCall { .. } => 2, // header 行 + prompt 第一行
        _ => return 0,
    };
    rows.min(2).min(block_h)
}

/// "[show in new tab]" 按钮文字长度
const OPEN_TAB_BUTTON_LEN: usize = 18; // "[show in new tab]"
/// 按钮右侧间距（与 build_header_with_button 的 right_gap 一致）
const OPEN_TAB_RIGHT_GAP: usize = 2;

/// Check if a mouse click hits the "[show in new tab]" button on an AgentCall block's header line.
/// Returns the sub_session_id if the button was hit.
///
/// `virtual_row` is the scroll-adjusted row, `column` is the screen column within the chat area
/// (adjusted for content area offset), `content_width` is the render width (= cache_width).
pub(crate) fn agent_open_tab_hit_test(
    messages: &[ChatLine],
    caches: &[crate::app::MessageCache],
    virtual_row: u16,
    column: u16,
    content_width: u16,
) -> Option<String> {
    let mut y: u16 = 0;
    for msg in messages {
        let style = theme::style_for(msg.line_type.clone());
        if let Some(cache) = caches.iter().find(|c| c.msg_id == msg.id) {
            let h = style.total_height(cache.line_count);
            if virtual_row >= y && virtual_row < y + h {
                // Must be an AgentCall on the header line
                // header 行 = y + top_margin + margin_vertical（与 render_block 一致）
                if matches!(msg.line_type, LineType::AgentCall { .. })
                    && virtual_row == y + style.top_margin + style.margin_vertical
                    && let Some(ref sub_sid) = msg.sub_session_id
                {
                    // 按钮位置：margin_horizontal(1) + content_width - right_gap - button_len
                    // 到 margin_horizontal(1) + content_width - right_gap
                    let button_start = 1u16
                        + content_width
                            .saturating_sub(OPEN_TAB_RIGHT_GAP as u16 + OPEN_TAB_BUTTON_LEN as u16);
                    let button_end = 1u16 + content_width.saturating_sub(OPEN_TAB_RIGHT_GAP as u16);
                    if column >= button_start && column < button_end {
                        return Some(sub_sid.clone());
                    }
                }
                break;
            }
            y += h;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_tool_args_summary: write_file ──────────────

    #[test]
    fn test_args_summary_write_file() {
        let args = r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#;
        let summary = format_tool_args_summary("write_file", args);
        assert_eq!(summary, "src/main.rs: 1");
    }

    #[test]
    fn test_args_summary_write_file_empty_content() {
        let args = r#"{"path":"empty.txt","content":""}"#;
        let summary = format_tool_args_summary("write_file", args);
        assert_eq!(summary, "empty.txt: 0");
    }

    #[test]
    fn test_args_summary_write_file_missing_content() {
        let args = r#"{"path":"no_content.rs"}"#;
        let summary = format_tool_args_summary("write_file", args);
        assert_eq!(summary, "no_content.rs: 0");
    }

    #[test]
    fn test_args_summary_write_file_invalid_json() {
        let summary = format_tool_args_summary("write_file", "not json");
        assert_eq!(summary, "not json");
    }

    // ── result_summary: write_file ────────────────────────

    #[test]
    fn test_result_summary_write_file() {
        let content = "Written 20 bytes to src/main.rs\nfn main() {}\n";
        let summary = result_summary("write_file", content);
        assert_eq!(summary, "Written 20 bytes (1 lines)");
    }

    #[test]
    fn test_result_summary_write_file_no_content_body() {
        let content = "Written 0 bytes to empty.txt\n";
        let summary = result_summary("write_file", content);
        assert_eq!(summary, "Written 0 bytes (0 lines)");
    }

    #[test]
    fn test_result_summary_write_file_error_result() {
        // 错误结果不是 "Written..." 开头，应返回空
        let content = "Failed to write file: permission denied";
        let summary = result_summary("write_file", content);
        assert_eq!(summary, "");
    }

    // ── format_tool_args_summary: edit_file ───────────────

    #[test]
    fn test_args_summary_edit_file() {
        let args = r#"{"path":"src/lib.rs","old_string":"foo","new_string":"bar\nbaz"}"#;
        let summary = format_tool_args_summary("edit_file", args);
        assert_eq!(summary, "src/lib.rs");
    }

    #[test]
    fn test_args_summary_edit_file_invalid_json() {
        let summary = format_tool_args_summary("edit_file", "bad json");
        assert_eq!(summary, "bad json");
    }

    // ── format_tool_args_summary: bash ────────────────────

    #[test]
    fn test_args_summary_bash() {
        let args = r#"{"command":"ls -la"}"#;
        let summary = format_tool_args_summary("bash", args);
        assert_eq!(summary, "ls -la");
    }

    #[test]
    fn test_args_summary_bash_long_command() {
        let long_cmd = "a".repeat(70);
        let args = format!(r#"{{"command":"{}"}}"#, long_cmd);
        let summary = format_tool_args_summary("bash", &args);
        assert_eq!(summary, format!("{}...", "a".repeat(57)));
    }

    #[test]
    fn test_args_summary_bash_missing_command() {
        let args = r#"{"timeout":30}"#;
        let summary = format_tool_args_summary("bash", args);
        assert_eq!(summary, "");
    }

    #[test]
    fn test_args_summary_bash_invalid_json() {
        let summary = format_tool_args_summary("bash", "bad json");
        assert_eq!(summary, "bad json");
    }

    // ── format_tool_args_full ─────────────────────────────

    #[test]
    fn test_args_full_bash_long_command() {
        let long_cmd = "a".repeat(70);
        let args = format!(r#"{{"command":"{}","timeout":120}}"#, long_cmd);
        let full = format_tool_args_full("bash", &args);
        assert_eq!(full, long_cmd);
        assert!(!full.contains("..."));
    }

    #[test]
    fn test_args_full_edit_file() {
        let args = r#"{"path":"src/lib.rs","old_string":"foo\nbar","new_string":"baz"}"#;
        let full = format_tool_args_full("edit_file", args);
        assert!(full.contains("path: src/lib.rs"));
        assert!(full.contains("--- old"));
        assert!(full.contains("foo\nbar"));
        assert!(full.contains("+++ new"));
        assert!(full.contains("baz"));
    }

    #[test]
    fn test_args_full_write_file() {
        let args = r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#;
        let full = format_tool_args_full("write_file", args);
        assert!(full.contains("path: src/main.rs"));
        assert!(full.contains("fn main() {}"));
    }

    #[test]
    fn test_args_full_read_file_with_lines() {
        let args = r#"{"path":"src/lib.rs","start_line":10,"end_line":20}"#;
        let full = format_tool_args_full("read_file", args);
        assert_eq!(full, "src/lib.rs:10-20");
    }

    #[test]
    fn test_args_full_other_tool() {
        let args = r#"{"pattern":"fn main","include":"*.rs"}"#;
        let full = format_tool_args_full("grep", args);
        assert!(full.contains("pattern: fn main"));
        assert!(full.contains("include: *.rs"));
    }

    #[test]
    fn test_args_full_invalid_json() {
        let full = format_tool_args_full("bash", "not json");
        assert_eq!(full, "not json");
    }

    // ── render_tool_call: expanded 显示完整参数 ────────────

    #[test]
    fn test_render_tool_call_expanded_shows_full_command() {
        let long_cmd =
            "cargo build --release --target x86_64-unknown-linux-gnu --features foo bar baz qux"
                .to_string();
        assert!(long_cmd.chars().count() > 60);
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::ToolCall {
                name: "bash".into(),
            },
            content: format!(r#"{{"command":"{}","timeout":120}}"#, long_cmd),
            call_id: Some("call-1".into()),
            tool_result: Some("done".into()),
            tool_error: false,
            sub_session_id: None,
        };

        // 展开态：包含完整命令，不含截断标记
        let expanded_lines = render_tool_call(&msg, 80, true);
        let expanded_text: String = expanded_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.trim_end())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            expanded_text.contains(&long_cmd),
            "expanded should contain full command"
        );

        // 收起态：仍是截断摘要，不含完整命令
        let collapsed_lines = render_tool_call(&msg, 80, false);
        let collapsed_text: String = collapsed_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !collapsed_text.contains(&long_cmd),
            "collapsed should not contain full command"
        );
        assert!(
            collapsed_text.contains("..."),
            "collapsed keeps truncated summary"
        );
    }

    #[test]
    fn test_render_tool_call_expanded_no_result_shows_args() {
        // 工具尚未返回结果时，展开也应显示完整参数
        let long_cmd = "b".repeat(70);
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::ToolCall {
                name: "bash".into(),
            },
            content: format!(r#"{{"command":"{}"}}"#, long_cmd),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: None,
        };
        let lines = render_tool_call(&msg, 80, true);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains(&long_cmd));
    }

    // ── count_diff_lines ──────────────────────────────────

    #[test]
    fn test_count_diff_lines_basic() {
        let diff = "Replaced 1 occurrence in src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n-old\n+new\n+new2\n";
        assert_eq!(count_diff_lines(diff), (1, 2));
    }

    #[test]
    fn test_count_diff_lines_context_lines() {
        // context 行不以 + 或 - 开头，不计入
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n same\n-old\n+new\n same\n";
        assert_eq!(count_diff_lines(diff), (1, 1));
    }

    #[test]
    fn test_count_diff_lines_no_changes() {
        let diff = "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n same\n";
        assert_eq!(count_diff_lines(diff), (0, 0));
    }

    // ── result_summary: edit_file ─────────────────────────

    #[test]
    fn test_result_summary_edit_file_success() {
        let content = "Replaced 1 occurrence in src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n-old\n+new\n+new2\n";
        let summary = result_summary("edit_file", content);
        assert_eq!(summary, "1 lines remove, 2 lines add");
    }

    #[test]
    fn test_result_summary_edit_file_error() {
        let content = "No matches found for 'foo'";
        let summary = result_summary("edit_file", content);
        assert_eq!(summary, "");
    }

    // ── render_agent_call ─────────────────────────────────

    #[test]
    fn test_render_agent_call_collapsed() {
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"Find all TODOs\nIn the codebase"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("abc123def456".into()),
        };
        let lines = render_agent_call(&msg, 60, false);
        // Header + first line of prompt
        assert_eq!(lines.len(), 2);
        // Header contains agent name and short session ID
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("explorer"));
        assert!(header.contains("abc123de"));
        assert!(header.contains("[show in new tab]"));
        // Second line is the first line of prompt
        let prompt_line: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(prompt_line.contains("Find all TODOs"));
    }

    #[test]
    fn test_render_agent_call_expanded() {
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "fixer".into(),
            },
            content: r#"{"prompt":"Fix the bug\nIn module X"}"#.into(),
            call_id: Some("call-2".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("xyz789abc".into()),
        };
        let lines = render_agent_call(&msg, 60, true);
        // Header + 2 prompt lines
        assert_eq!(lines.len(), 3);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("fixer"));
        assert!(header.contains("[show in new tab]"));
        // Full prompt is shown
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("Fix the bug"));
        assert!(all_text.contains("In module X"));
    }

    #[test]
    fn test_render_agent_call_no_sub_session_id() {
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"Search"}"#.into(),
            call_id: Some("call-3".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: None,
        };
        let lines = render_agent_call(&msg, 60, false);
        assert_eq!(lines.len(), 2);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("..."));
    }

    #[test]
    fn test_build_header_with_button() {
        let result = build_header_with_button("hello", "[btn]", 20);
        assert!(result.starts_with("hello"));
        assert!(result.ends_with("[btn]"));
        // right_gap=2: total = left_len + gap + button_len = 5 + 8 + 5 = 18
        assert_eq!(result.chars().count(), 18);
    }

    #[test]
    fn test_build_header_with_button_too_narrow() {
        let result = build_header_with_button("hello world this is long", "[btn]", 10);
        // Should just be truncated left text, no button
        assert!(!result.contains("[btn]"));
    }

    // ── agent_open_tab_hit_test ───────────────────────────

    #[test]
    fn test_agent_open_tab_hit_test_button_hit() {
        use crate::app::MessageCache;
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"test"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("sub-sess-123".into()),
        };
        let cache = MessageCache::from_message(&msg, 60, false, None);
        let messages = vec![msg];
        let caches = vec![cache];
        // AGENT_CALL_STYLE: top_margin=1, margin_vertical=1 -> header at y+2
        let result = agent_open_tab_hit_test(&messages, &caches, 2, 55, 60);
        assert_eq!(result.as_deref(), Some("sub-sess-123"));
    }

    #[test]
    fn test_agent_open_tab_hit_test_button_missed() {
        use crate::app::MessageCache;
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"test"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("sub-sess-123".into()),
        };
        let cache = MessageCache::from_message(&msg, 60, false, None);
        let messages = vec![msg];
        let caches = vec![cache];
        // Click on header row but left side (not the button)
        let result = agent_open_tab_hit_test(&messages, &caches, 2, 5, 60);
        assert!(result.is_none());
    }

    #[test]
    fn test_agent_open_tab_hit_test_non_header_row() {
        use crate::app::MessageCache;
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"test"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("sub-sess-123".into()),
        };
        let cache = MessageCache::from_message(&msg, 60, false, None);
        let messages = vec![msg];
        let caches = vec![cache];
        // Click on row 3 (prompt line, not header which is at row 2)
        let result = agent_open_tab_hit_test(&messages, &caches, 3, 55, 60);
        assert!(result.is_none());
    }

    // ── tool_block_hit_test: 仅 header 区域可点击 ──────────────

    #[test]
    fn test_tool_block_hit_test_agent_call_header_rows_only() {
        use crate::app::MessageCache;
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"Find all TODOs\nIn the codebase"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("sub-sess-123".into()),
        };
        let cache = MessageCache::from_message(&msg, 60, false, None);
        let style = crate::theme::style_for(LineType::AgentCall {
            name: "explorer".into(),
        });
        let h = style.total_height(cache.line_count);
        let messages = vec![msg];
        let caches = vec![cache];
        let expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

        let start = style.top_margin + style.margin_vertical;
        // header 行（agent_name）与 prompt 第一行可点击
        for row in [start, start + 1] {
            assert!(row < h, "row {} should be inside block (h={})", row, h);
            let result = tool_block_hit_test(&messages, &caches, row, 60);
            assert_eq!(
                result.as_deref(),
                Some("call-1"),
                "row {} should hit the header region (h={})",
                row,
                h
            );
        }
        // prompt 第二行及之后（result 行、留白）不可点击
        for row in (start + 2)..h {
            let result = tool_block_hit_test(&messages, &caches, row, 60);
            assert!(
                result.is_none(),
                "row {} should NOT hit the header region (h={})",
                row,
                h
            );
        }
        // Row h should NOT hit
        assert!(tool_block_hit_test(&messages, &caches, h, 60).is_none());
    }

    #[test]
    fn test_tool_block_hit_test_tool_call_header_rows_only() {
        use crate::app::MessageCache;
        let msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::ToolCall {
                name: "bash".into(),
            },
            content: r#"{"command":"cargo build"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: Some("ok".into()),
            tool_error: false,
            sub_session_id: None,
        };
        let cache = MessageCache::from_message(&msg, 60, false, None);
        let style = crate::theme::style_for(LineType::ToolCall {
            name: "bash".into(),
        });
        let h = style.total_height(cache.line_count);
        let messages = vec![msg];
        let caches = vec![cache];
        let expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

        let start = style.top_margin + style.margin_vertical;
        // 折叠态：仅 header 摘要行（tool_name + tool args）可点击
        let result = tool_block_hit_test(&messages, &caches, start, 60);
        assert_eq!(result.as_deref(), Some("call-1"), "header row should hit");
        // result summary 行不可点击
        for row in (start + 1)..h {
            let result = tool_block_hit_test(&messages, &caches, row, 60);
            assert!(
                result.is_none(),
                "row {} should NOT hit when collapsed (h={})",
                row,
                h
            );
        }

        // 展开态：可点击区域仍为前两行（header 摘要行）；args / results
        // 区域留给文本选择，点击不再触发折叠
        let mut expanded_set = std::collections::HashSet::new();
        expanded_set.insert("call-1".to_string());
        let header_rows =
            crate::app::wrap_text(&format!("{} {} {}", tool_icon("bash"), "bash", "command: cargo build"), 60).len() as u16;
        let clickable_rows = header_rows.min(2);
        for row in start..(start + clickable_rows) {
            let result = tool_block_hit_test(&messages, &caches, row, 60);
            assert_eq!(
                result.as_deref(),
                Some("call-1"),
                "row {} should hit when expanded (h={})",
                row,
                h
            );
        }
        for row in (start + clickable_rows)..h {
            let result = tool_block_hit_test(&messages, &caches, row, 60);
            assert!(
                result.is_none(),
                "row {} should NOT hit when expanded (h={})",
                row,
                h
            );
        }
    }

    #[test]
    fn test_tool_block_hit_test_after_user_message() {
        // Simulate: User message + AgentCall, click on prompt row of AgentCall
        use crate::app::MessageCache;
        let user_msg = ChatLine {
            id: 0,
            version: 0,
            line_type: LineType::User,
            content: "Find TODOs please".into(),
            call_id: None,
            tool_result: None,
            tool_error: false,
            sub_session_id: None,
        };
        let agent_msg = ChatLine {
            id: 1,
            version: 0,
            line_type: LineType::AgentCall {
                name: "explorer".into(),
            },
            content: r#"{"prompt":"Find all TODOs"}"#.into(),
            call_id: Some("call-1".into()),
            tool_result: None,
            tool_error: false,
            sub_session_id: Some("sub-sess-123".into()),
        };
        let user_cache = MessageCache::from_message(&user_msg, 60, false, None);
        let agent_cache = MessageCache::from_message(&agent_msg, 60, false, None);
        let messages = vec![user_msg, agent_msg];
        let caches = vec![user_cache, agent_cache];

        let user_style = crate::theme::style_for(LineType::User);
        let user_h =
            user_style.total_height(caches.iter().find(|c| c.msg_id == 0).unwrap().line_count);
        let agent_style = crate::theme::style_for(LineType::AgentCall {
            name: "explorer".into(),
        });
        let _agent_h =
            agent_style.total_height(caches.iter().find(|c| c.msg_id == 1).unwrap().line_count);

        // Click on header row of AgentCall (first content line = y + top_margin + margin_vertical)
        let header_row = user_h + agent_style.top_margin + agent_style.margin_vertical;
        let expanded: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(
            tool_block_hit_test(&messages, &caches, header_row, 60).as_deref(),
            Some("call-1"),
            "header row should hit"
        );

        // Click on prompt row of AgentCall (second content line)
        let prompt_row = header_row + 1;
        assert_eq!(
            tool_block_hit_test(&messages, &caches, prompt_row, 60).as_deref(),
            Some("call-1"),
            "prompt row should hit"
        );

        // Click on result row (third content line) should NOT toggle
        assert!(
            tool_block_hit_test(&messages, &caches, prompt_row + 1, 60).is_none(),
            "result row should not hit"
        );

        // agent_open_tab_hit_test should return None for prompt row (not a button click)
        assert!(
            agent_open_tab_hit_test(&messages, &caches, prompt_row, 55, 60).is_none(),
            "agent_open_tab_hit_test should return None for prompt row"
        );
    }
}
