#![allow(dead_code)]
#![allow(clippy::bool_assert_comparison)]

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::ListState,
};
use ratatui_textarea::WrapMode;
use std::path::Path;
use unicode_width::UnicodeWidthChar;

use crate::connection::ConnState;
use crate::image::{ImageHeightInfo, ImageMetrics, ImageState};
use crate::notify::NotifyEngine;
use crate::theme;
use visp_proto::visp::{ServerMessage, server_message};

/// 检测内容是否为 diff 格式（git diff, diff -u 等输出）
fn detect_diff(content: &str) -> Option<()> {
    let mut lines = content.lines();
    // 检查是否以 diff --git 开头（git diff）
    if lines.clone().any(|l| l.starts_with("diff --git")) {
        return Some(());
    }
    // 或检查是否有 --- / +++ 配对（context/unified diff）
    let has_three_dashes = lines.clone().any(|l| l.starts_with("--- "));
    let has_plus_plus = lines.any(|l| l.starts_with("+++ "));
    if has_three_dashes && has_plus_plus {
        return Some(());
    }
    None
}

/// 滚动状态（替代 tui-scrollview）
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollState {
    pub x: u16,
    pub y: u16,
}

impl ScrollState {
    pub fn scroll_up(&mut self) {
        self.y = self.y.saturating_sub(1);
    }
    pub fn scroll_down(&mut self) {
        self.y = self.y.saturating_add(1);
    }
}

/// 滚动条几何快照（渲染时记录，供鼠标拖拽命中测试）
#[derive(Clone, Copy, Debug)]
pub struct ScrollbarGeo {
    /// 滚动条 1 列矩形（屏幕坐标）
    pub area: Rect,
    /// 滑块相对轨道顶部的偏移
    pub thumb_start: u16,
    /// 滑块长度
    pub thumb_len: u16,
    /// 最大滚动偏移（total_lines - visible）
    pub max_scroll: u16,
}

/// 计算滑块几何，与 ratatui `Scrollbar` 组件内部公式一致（rounding_divide
/// 为四舍五入整除）。`content_length` 语义取"可滚动位置数"（max_scroll + 1），
/// 保证滚到底时滑块贴住轨道底部。
/// 返回 (thumb_start, thumb_len)。
pub(crate) fn scrollbar_thumb_geometry(
    total_lines: u16,
    visible: u16,
    scroll_y: u16,
    track: u16,
) -> (u16, u16) {
    let max_scroll = total_lines.saturating_sub(visible);
    let (track, visible, scroll) = (track as u32, visible as u32, scroll_y as u32);
    let max_vp = max_scroll as u32 + visible; // max_position + viewport_length
    if track == 0 || max_vp == 0 {
        return (0, track as u16);
    }
    let rd = |n: u32, d: u32| (n + d / 2) / d;
    let thumb_len = rd(visible * track, max_vp).clamp(1, track);
    let thumb_start = rd(scroll * track, max_vp).clamp(0, track - thumb_len);
    (thumb_start as u16, thumb_len as u16)
}

/// 由目标滑块顶部偏移反推滚动偏移（拖拽映射，`scrollbar_thumb_geometry` 的逆运算）
pub(crate) fn scrollbar_scroll_from_thumb(
    thumb_start: u16,
    track: u16,
    visible: u16,
    max_scroll: u16,
) -> u16 {
    let (track, visible) = (track as u32, visible as u32);
    let max_vp = max_scroll as u32 + visible;
    if track == 0 || max_vp == 0 {
        return 0;
    }
    let scroll = (thumb_start as u32 * max_vp + track / 2) / track;
    (scroll as u16).min(max_scroll)
}

/// 用 syntect 高亮代码块，返回 ratatui 行
pub(crate) fn highlight_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    use syntect::easy::HighlightLines;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;
    use syntect::util::LinesWithEndings;

    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];

    let syntax = if lang.is_empty() {
        // 先检测 diff 格式（git diff / diff -u 等输出）
        detect_diff(code)
            .and_then(|_| ss.find_syntax_by_name("diff"))
            .or_else(|| {
                // 再尝试从 shebang（#!/usr/bin/env python 等）检测语言
                ss.find_syntax_by_first_line(code)
            })
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    } else {
        ss.find_syntax_by_token(lang)
            .or_else(|| ss.find_syntax_by_name(lang))
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    };

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    for line in LinesWithEndings::from(code) {
        if let Ok(ranges) = h.highlight_line(line, &ss) {
            let spans: Vec<Span<'static>> = ranges
                .iter()
                .map(|(syn_style, text)| {
                    let fg = ratatui::style::Color::Rgb(
                        syn_style.foreground.r,
                        syn_style.foreground.g,
                        syn_style.foreground.b,
                    );
                    Span::styled(text.to_string(), Style::default().fg(fg))
                })
                .collect();
            // LinesWithEndings preserves trailing \n; remove it
            let line_text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            if line_text.ends_with('\n') {
                // 去掉最后的换行，重新构建不带 \n 的 spans
                let trimmed: Vec<Span<'static>> = ranges
                    .iter()
                    .map(|(syn_style, text)| {
                        let t = text.trim_end_matches('\n');
                        let fg = ratatui::style::Color::Rgb(
                            syn_style.foreground.r,
                            syn_style.foreground.g,
                            syn_style.foreground.b,
                        );
                        Span::styled(t.to_string(), Style::default().fg(fg))
                    })
                    .collect();
                result.push(Line::from(trimmed));
            } else {
                result.push(Line::from(spans));
            }
        }
    }
    result
}

/// 在 markdown 中查找代码块并用唯一标记替换，返回 (处理后的文本, 高亮行列表)
fn process_code_blocks(md: &str) -> (String, Vec<Vec<Line<'static>>>) {
    use regex::Regex;
    let re = Regex::new(r"(?ms)```(\w*)\n(.*?)```").unwrap();
    let mut highlighted: Vec<Vec<Line<'static>>> = Vec::new();
    let mut result = String::new();
    let mut last_end = 0;

    for cap in re.captures_iter(md) {
        let full_match = cap.get(0).unwrap();
        let lang = cap.get(1).map_or("", |m| m.as_str());
        let code = cap.get(2).map_or("", |m| m.as_str());

        // 追加匹配前的文本
        result.push_str(&md[last_end..full_match.start()]);

        // 高亮代码
        let lines = highlight_code_block(lang, code);
        let idx = highlighted.len();
        highlighted.push(lines);

        // 插入标记（使用控制字符确保不被 markdown 解析器干扰）
        result.push_str(&format!("\x00CODEBLOCK_{}\x00", idx));
        last_end = full_match.end();
    }
    // 追加剩余文本
    result.push_str(&md[last_end..]);
    (result, highlighted)
}

/// 将一个带样式的 Line 按屏幕宽度拆分为多行，保留样式，每行末尾补空格到指定宽度
fn wrap_styled_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    use std::collections::VecDeque;

    struct StyledChar {
        ch: char,
        style: Style,
        cw: usize,
    }

    // 展平 spans 为字符序列
    let mut chars: VecDeque<StyledChar> = VecDeque::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            chars.push_back(StyledChar {
                ch,
                style: span.style,
                cw,
            });
        }
    }

    if chars.is_empty() {
        return vec![Line::from(" ")];
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut line_spans: Vec<(String, Style)> = Vec::new();
    let mut col: usize = 0;

    while let Some(sc) = chars.pop_front() {
        if sc.ch == '\n' {
            // 显式换行
            flush_line(&mut line_spans, width, &mut result, col);
            col = 0;
            continue;
        }
        if col + sc.cw > width && col > 0 {
            flush_line(&mut line_spans, width, &mut result, col);
            col = 0;
        }
        // 与最后一个 span 样式相同则合并，否则新建
        if let Some((text, last_style)) = line_spans.last_mut()
            && *last_style == sc.style
        {
            text.push(sc.ch);
        } else {
            line_spans.push((sc.ch.to_string(), sc.style));
        }
        col += sc.cw;
    }

    // 最后一行
    if !line_spans.is_empty() {
        let mut spans: Vec<Span<'static>> = line_spans
            .into_iter()
            .map(|(t, s)| Span::styled(t, s))
            .collect();
        // 补空格到宽度
        if col < width {
            spans.push(Span::styled(" ".repeat(width - col), Style::default()));
        }
        result.push(Line::from(spans));
    }

    result
}

fn flush_line(
    line_spans: &mut Vec<(String, Style)>,
    width: usize,
    result: &mut Vec<Line<'static>>,
    col: usize,
) {
    let mut spans: Vec<Span<'static>> = line_spans
        .drain(..)
        .map(|(t, s)| Span::styled(t, s))
        .collect();
    if col < width {
        spans.push(Span::styled(" ".repeat(width - col), Style::default()));
    }
    result.push(Line::from(spans));
}

#[derive(Debug, Clone, PartialEq)]
pub enum LineType {
    User,
    Assistant,
    Thinking,
    ToolCall {
        name: String,
    },
    ToolResult {
        name: String,
    },
    ToolError {
        name: String,
    },
    AgentCall {
        name: String,
    },
    Error,
    Status,
    /// 独立验证进度/结论
    Verification,
    Usage,
    Image {
        path: String,
        alt_text: String,
        remote_url: Option<String>,
    },
}

/// 拖拽选择高亮用的屏幕行归属表（每次渲染重建）：
/// - `content`：行 → block 内容列范围 `(x_start, x_end)`（高亮只涂内容列）
/// - `blank`：block 内的留白行（边距 / 内边距 / 阴影行），高亮时整行跳过
///
/// 不在表中的行（block 之间的空气行、输入区等）保持全宽高亮。
#[derive(Debug, Default, Clone)]
pub struct BlockRows {
    pub content: std::collections::HashMap<u16, (u16, u16)>,
    pub blank: std::collections::HashSet<u16>,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub id: u64,
    pub version: u64,
    pub line_type: LineType,
    pub content: String,
    pub call_id: Option<String>,
    /// Result content merged from ToolResult (for collapsible ToolCall blocks)
    pub tool_result: Option<String>,
    /// Whether the result was an error
    pub tool_error: bool,
    /// 子 Agent 的 session ID（仅 AgentCall 类型，用于"打开 tab"按钮）
    pub sub_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Error,
    ViewOnly,
}

#[derive(Debug, Clone)]
pub struct TabEntry {
    pub session_id: String,
    pub agent_name: String,
    pub status: AgentStatus,
    pub frames: Vec<ServerMessage>,
    pub messages: Vec<ChatLine>,
    pub rendered_up_to: usize,
    pub streaming_text: String,
    pub generating: bool,
    /// 当前流式输出的起始时间（首个 TextDelta 到达时启用），用于估算输出速率
    pub stream_started_at: Option<std::time::Instant>,
    /// 本次生成中 provider 逐事件上报的输出 token 增量之和
    /// （仅供实时速率展示；结算仍以 UsageInfo 为准）
    pub stream_output_tokens: u64,
    /// 流结束时冻结的实时估算速率。仅作 assistant 统计行最终统计速度的
    /// 兜底（结算数据缺失时），主指标见 `last_stream_elapsed` +
    /// `final_token_speed`。实时路径中 Done 帧会先经 `stop_generating`
    /// 清空流式计时器、再渲染统计行（`consume_pending_usage`），此时钟
    /// 已不存在，需用冻结值兜底，否则统计行 t/s 恒为 "--"。
    pub last_stream_tps: Option<f64>,
    /// 流结束时冻结的总耗时（秒）。assistant 统计行的速率是**最终统计
    /// 速度** = 结算输出 tokens（UsageInfo）/ 本次流总耗时，与输入栏的
    /// 实时速率（`tokens_per_second`，流式估算）是两个不同指标（ui.md）。
    pub last_stream_elapsed: Option<f64>,
    pub pending_usage: Option<(u32, u32, u32, u32, u32)>,
    pub next_message_id: u64,
    pub scroll: usize,
    /// 是否为主 tab（不可关闭）
    pub is_main: bool,
    /// 子 agent 的 task prompt（渲染时画为子 tab 第一行，不进 messages）
    pub task_prompt: Option<String>,
}

impl TabEntry {
    pub fn new(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_name: agent_name.into(),
            status: AgentStatus::Running,
            frames: Vec::new(),
            messages: Vec::new(),
            rendered_up_to: 0,
            streaming_text: String::new(),
            generating: false,
            stream_started_at: None,
            stream_output_tokens: 0,
            last_stream_tps: None,
            last_stream_elapsed: None,
            pending_usage: None,
            next_message_id: 0,
            scroll: 0,
            is_main: false,
            task_prompt: None,
        }
    }

    pub fn new_view_only(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        let mut entry = Self::new(session_id, agent_name);
        entry.status = AgentStatus::ViewOnly;
        entry
    }

    /// 重连恢复清理：复位单 tab 的全部本地渲染/流式状态（设计 §5.9）。
    ///
    /// `frames`/`rendered_up_to` 是 `render_pending` 依赖的未渲染队列与游标
    /// 不变量，必须一并复位；`messages` 也在此清空——主 session 的历史由随后
    /// `send_join` 的权威回放重建，保留旧消息会与回放内容重复。
    pub fn reset_after_reconnect(&mut self) {
        self.frames.clear();
        self.rendered_up_to = 0;
        self.streaming_text.clear();
        self.stream_started_at = None;
        self.stream_output_tokens = 0;
        self.pending_usage = None;
        self.last_stream_tps = None;
        self.last_stream_elapsed = None;
        self.generating = false;
        // 本地已渲染历史交由权威回放重建
        self.messages.clear();
        self.next_message_id = 0;
        self.scroll = 0;
    }

    /// Returns the streaming text with incomplete image markers truncated.
    /// If there's an incomplete `<image: ...` marker at the end (no closing `>`),
    /// the text is truncated to before the `<image:` prefix.
    pub fn streaming_display_text(&self) -> String {
        let text = &self.streaming_text;
        // Search for the last `<image: ` occurrence
        if let Some(pos) = text.rfind("<image: ") {
            // Check if there's a closing `>` after this position
            let after_marker = &text[pos..];
            if !after_marker.contains('>') {
                // Incomplete marker: truncate
                return text[..pos].to_string();
            }
        }
        text.clone()
    }

    pub fn push_chat_line(
        &mut self,
        line_type: LineType,
        content: String,
        call_id: Option<String>,
    ) {
        let id = self.next_message_id;
        self.next_message_id += 1;
        self.messages.push(ChatLine {
            id,
            version: 0,
            line_type,
            content,
            call_id,
            tool_result: None,
            tool_error: false,
            sub_session_id: None,
        });
    }

    /// 推送一行 Status 提示，但若最后一行已是**完全相同文本**的 Status 则跳过。
    /// 用于忙拒绝（SessionBusy）提示：活跃 tab 路径会先经 `route_frame` 追加
    /// 一次、再经 `render_pending` 处理同一帧追加第二次，去重后可收敛为一行；
    /// 只比较紧邻的末行，不影响其它行类型或其他位置的 Status。
    pub fn push_status_line_dedup(&mut self, content: String) {
        if let Some(last) = self.messages.last()
            && last.line_type == LineType::Status
            && last.content == content
        {
            return;
        }
        self.push_chat_line(LineType::Status, content, None);
    }

    pub fn push_chat_lines(&mut self, lines: Vec<ChatLine>) {
        for mut line in lines {
            line.id = self.next_message_id;
            line.version = 0;
            self.next_message_id += 1;
            self.messages.push(line);
        }
    }

    pub fn flush_streaming(&mut self) {
        self.stream_started_at = None;
        self.stream_output_tokens = 0;
        self.last_stream_tps = None;
        self.last_stream_elapsed = None;
        if !self.streaming_text.is_empty() {
            let text = std::mem::take(&mut self.streaming_text);
            let lines = crate::image::split_image_markers(&text, LineType::Assistant);
            self.push_chat_lines(lines);
        }
    }

    /// 停止生成：冻结最终统计速度所需的总耗时与实时估算兜底值，
    /// 并清除 generating 标记与流式计时器
    pub fn stop_generating(&mut self) {
        // 冻结再清时钟：实时路径（event.rs）在渲染 Done 帧前调用本方法，
        // 统计行渲染（consume_pending_usage）时实时速率已不可得。
        // 统计行主指标是最终统计速度（结算输出 / 冻结总耗时，
        // 见 final_token_speed）；冻结的实时估算仅作结算数据缺失时的兜底
        self.last_stream_tps = self.tokens_per_second();
        self.last_stream_elapsed = self.stream_started_at.map(|s| s.elapsed().as_secs_f64());
        self.generating = false;
        self.stream_started_at = None;
        self.stream_output_tokens = 0;
    }

    /// 输出速率（ui.md 输入栏设计）：取「provider 上报的输出增量」
    /// （`AppState::apply_usage_delta` 累加的 `stream_output_tokens`）与
    /// 「字符数 / 4 ≈ token 数」估算的**较大者**，而非上报值短路。
    /// 融合的原因：
    /// - Anthropic `message_start` 携带 `output_tokens: 1`，若上报值短路，
    ///   整个流会显示 `1/elapsed` 的近似冻结假速率；
    /// - 推理（thinking）阶段没有 TextDelta，需要用 thinking 内容估算；
    /// - 纯工具调用流无文本但有上报增量，用上报值。
    ///
    /// 估算来源：`streaming_text` + 最后一条 Thinking 消息（仅当其为最新
    /// 消息时计入，避免把上一轮遗留的 thinking 计入新一轮速率）。
    /// 流刚开始（< 0.5s，避免速率尖峰）或两种数据都没有时返回 None。
    pub fn tokens_per_second(&self) -> Option<f64> {
        let started = self.stream_started_at?;
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed < 0.5 {
            return None;
        }
        let mut estimated_tokens = self.streaming_text.chars().count() as f64 / 4.0;
        if let Some(last) = self.messages.last()
            && let LineType::Thinking = last.line_type
        {
            let content = last
                .content
                .strip_prefix("[Thinking] ")
                .unwrap_or(&last.content);
            estimated_tokens += content.chars().count() as f64 / 4.0;
        }
        let tokens = self.stream_output_tokens as f64;
        let tokens = tokens.max(estimated_tokens);
        if tokens > 0.0 {
            Some(tokens / elapsed)
        } else {
            None
        }
    }

    /// 最终统计速度（ui.md 对话栏设计）：assistant 统计行的速率取
    /// 「结算输出 tokens（UsageInfo 的 output）」/「本次流总耗时」，
    /// 与输入栏的实时速率（[`Self::tokens_per_second`]，流式估算）是
    /// 两个不同指标。不受实时速率 0.5s 防尖峰守卫影响（快速回复也有值）。
    /// 结算数据缺失（输出 tokens 为 0 或无冻结/进行中耗时）时退回流结束
    /// 时冻结的实时估算；两者都不可得（如历史回放）时返回 None（显示 "--"）。
    pub fn final_token_speed(&self, output_tokens: u32) -> Option<f64> {
        let elapsed = self.last_stream_elapsed.or_else(|| {
            // 防御路径：统计行在时钟清空前渲染时直接取当前耗时
            self.stream_started_at.map(|s| s.elapsed().as_secs_f64())
        });
        if output_tokens > 0
            && let Some(elapsed) = elapsed.filter(|e| *e > 0.0)
        {
            return Some(f64::from(output_tokens) / elapsed);
        }
        self.last_stream_tps
    }

    /// 消费 pending_usage，将 token 统计追加到最后一条 Assistant 消息或 streaming_text。
    /// 用于回放时在 UserMessage 和 Done 帧处理中追加 token footer。
    pub fn consume_pending_usage(&mut self) {
        if let Some((it, ot, tc, ..)) = self.pending_usage.take() {
            let time = chrono::Local::now().format("%H:%M:%S");
            // ui.md 对话栏设计：统计行速率为最终统计速度（结算输出 tokens /
            // 总耗时），与输入栏实时速率是两个指标；两者都不可得（如历史
            // 回放）时显示 "--"
            let tps = match self.final_token_speed(ot) {
                Some(v) => format!("{v:.1}"),
                None => "--".to_string(),
            };
            let suffix =
                format!("\n\n[{time} | Tokens {it} in / {ot} out on {tps} t/s | Tools: {tc}]");
            if !self.streaming_text.is_empty() {
                // 有流式文本：追加到 streaming_text，flush 时一并成为本条 Assistant
                self.streaming_text.push_str(&suffix);
            } else if let Some(last) = self
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.line_type, LineType::Assistant))
            {
                // 已 flush（如 ToolCall 后）：追加到最后一条 Assistant 消息
                last.content.push_str(&suffix);
                last.version += 1;
            } else {
                // 没有任何 Assistant 消息：用 streaming_text 新建一条
                self.streaming_text.push_str(&suffix);
            }
        }
    }

    /// 消费一帧思考**增量**（相对同一思考块上一帧的新增文本）。
    /// 语义（proto `ThinkingBlock.thinking`）已改为增量，故：
    /// - 末行是 Thinking → 追加后缀，行内保持累积全文；
    /// - 否则新建 Thinking 行，且仅此时加一次 `[Thinking] ` 前缀。
    ///
    /// 行内始终为全文，`tokens_per_second` 的估算依赖此性质。
    pub fn update_thinking(&mut self, content: String) {
        if let Some(last) = self.messages.last_mut()
            && matches!(last.line_type, LineType::Thinking)
        {
            last.content.push_str(&content);
            last.version += 1;
        } else {
            self.push_chat_line(LineType::Thinking, format!("[Thinking] {content}"), None);
        }
    }

    /// 查找与指定 call_id 匹配的 ToolCall 消息，在其后追加结果内容。
    /// 若未找到匹配的 ToolCall，则创建一个新的 ToolCall 消息。
    pub fn insert_tool_result(&mut self, call_id: &str, content: String) {
        if let Some(msg) = self.messages.iter_mut().find(|m| {
            matches!(m.line_type, LineType::ToolCall { .. })
                && m.call_id.as_deref() == Some(call_id)
        }) {
            msg.tool_result = Some(content);
            msg.version += 1;
        } else {
            self.push_chat_line(
                LineType::ToolCall {
                    name: String::new(),
                },
                content,
                Some(call_id.to_string()),
            );
        }
    }

    /// 按 id 查找消息并更新内容，版本号 +1。
    pub fn update_message(&mut self, id: u64, content: String) {
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == id) {
            msg.content = content;
            msg.version += 1;
        }
    }

    /// 处理 self.frames 中尚未渲染的消息（从 rendered_up_to 开始）。
    /// 将 frames 中的 ServerMessage 转化为 ChatLine 追加到 self.messages。
    pub fn render_pending(&mut self) {
        use visp_proto::visp::server_message;

        while self.rendered_up_to < self.frames.len() {
            let msg = self.frames[self.rendered_up_to].clone();
            match msg.payload {
                Some(server_message::Payload::TextDelta(delta)) => {
                    if self.stream_started_at.is_none() {
                        self.stream_started_at = Some(std::time::Instant::now());
                    }
                    self.streaming_text.push_str(&delta.delta);
                }
                Some(server_message::Payload::ToolCall(tc)) => {
                    self.flush_streaming();
                    self.push_chat_line(
                        LineType::ToolCall { name: tc.tool_name },
                        tc.arguments,
                        Some(tc.call_id),
                    );
                }
                Some(server_message::Payload::ToolResult(tr)) => {
                    self.flush_streaming();
                    let tool_name = if !tr.tool_name.is_empty() {
                        tr.tool_name
                    } else {
                        self.messages
                            .iter()
                            .find(|m| {
                                matches!(
                                    &m.line_type,
                                    LineType::ToolCall { .. } | LineType::AgentCall { .. }
                                ) && m.call_id.as_deref() == Some(&tr.call_id)
                            })
                            .and_then(|m| match &m.line_type {
                                LineType::ToolCall { name } | LineType::AgentCall { name } => {
                                    Some(name.clone())
                                }
                                _ => None,
                            })
                            .unwrap_or_default()
                    };

                    // Merge into existing ToolCall/AgentCall ChatLine
                    if let Some(msg) = self.messages.iter_mut().find(|m| {
                        matches!(
                            &m.line_type,
                            LineType::ToolCall { .. } | LineType::AgentCall { .. }
                        ) && m.call_id.as_deref() == Some(&tr.call_id)
                    }) {
                        msg.tool_result = Some(tr.content);
                        msg.tool_error = tr.is_error;
                        msg.version += 1;
                    } else {
                        // Fallback: create separate ChatLines
                        let line_type = if tr.is_error {
                            LineType::ToolError {
                                name: tool_name.clone(),
                            }
                        } else {
                            LineType::ToolResult {
                                name: tool_name.clone(),
                            }
                        };
                        let lines = crate::image::split_image_markers(&tr.content, line_type);
                        for mut line in lines {
                            line.id = self.next_message_id;
                            line.call_id = Some(tr.call_id.clone());
                            self.next_message_id += 1;
                            self.messages.push(line);
                        }
                    }
                }
                Some(server_message::Payload::ThinkingBlock(tb)) => {
                    // thinking 也是输出：首个 thinking 块到达即启动速率时钟，
                    // 否则推理阶段 tokens_per_second 会因无时钟而一直显示 "--"
                    if self.stream_started_at.is_none() {
                        self.stream_started_at = Some(std::time::Instant::now());
                    }
                    // 增量语义：仅传增量文本，前缀与追加语义由 update_thinking 统一处理
                    self.update_thinking(tb.thinking);
                }
                Some(server_message::Payload::VerificationUpdate(vu)) => {
                    self.flush_streaming();
                    for line in format_verification_lines(&vu) {
                        self.push_chat_line(LineType::Verification, line, None);
                    }
                }
                Some(server_message::Payload::StatusUpdate(su)) => {
                    // NOTE: user_inputs handling deferred to later step
                    self.push_chat_line(LineType::Status, su.message, None);
                }
                Some(server_message::Payload::UserMessage(um)) => {
                    // 回放时重放用户消息：先消费 pending_usage 为上一条 assistant 追加 token 统计
                    self.consume_pending_usage();
                    self.flush_streaming();
                    let lines = crate::image::split_image_markers(&um.content, LineType::User);
                    self.push_chat_lines(lines);
                }
                Some(server_message::Payload::Error(err)) => {
                    if err.code == "SessionBusy" {
                        // 忙拒绝不代表本轮失败：不置 Error、不 flush 在途流式内容、
                        // 不提前放开输入门禁，仅追加一行提示（去重，避免与
                        // route_frame 的状态更新块重复）
                        self.push_status_line_dedup(err.message.clone());
                    } else {
                        self.flush_streaming();
                        self.push_chat_line(
                            LineType::Error,
                            format!("{}: {}", err.code, err.message),
                            None,
                        );
                        self.stop_generating();
                        self.status = AgentStatus::Error;
                    }
                }
                Some(server_message::Payload::Done(_)) => {
                    // 将 token 统计 + 时间戳追加到对话区
                    self.consume_pending_usage();
                    self.flush_streaming();
                    self.stop_generating();
                    if self.status == AgentStatus::Running {
                        self.status = AgentStatus::Done;
                    }
                }
                Some(server_message::Payload::ImageBlock(ib)) => {
                    self.flush_streaming();
                    let remote_url = if ib.remote_url.is_empty() {
                        None
                    } else {
                        Some(ib.remote_url.clone())
                    };
                    let path = ib.path.clone();
                    // Cache key: use path if non-empty, otherwise use remote_url
                    let cache_key = if path.is_empty() {
                        ib.remote_url.clone()
                    } else {
                        path.clone()
                    };
                    let alt_text = Path::new(&cache_key)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| cache_key.clone());
                    self.push_chat_line(
                        LineType::Image {
                            path,
                            alt_text,
                            remote_url,
                        },
                        String::new(),
                        None,
                    );
                }
                Some(server_message::Payload::ImageError(ie)) => {
                    self.flush_streaming();
                    self.push_chat_line(
                        LineType::Error,
                        format!("[图片加载失败: {}]", ie.reason),
                        None,
                    );
                }
                _ => {}
            }
            self.rendered_up_to += 1;
        }
    }
}

#[derive(Debug, Clone)]
pub struct TabBar {
    pub tabs: Vec<TabEntry>,
    pub active: usize,
    pub page_start: usize,
    pub last_term_width: u16,
    /// 上次渲染时 tabs widget content_area 的 x 坐标（屏幕绝对列）
    pub last_tab_area_x: u16,
    /// 上次渲染时 tabs widget content_area 的 y 坐标（tab 内容行，分隔线在 y+1）
    pub last_tab_area_y: u16,
    /// 已关闭的子 agent tab（按 session_id 索引），用于重新打开时恢复
    pub closed_tabs: Vec<TabEntry>,
    /// 尚未打开的子 agent tab（默认不自动创建活跃 tab，帧暂存于此）
    pub hidden_tabs: Vec<TabEntry>,
}

impl TabBar {
    pub fn new(main_session_id: String) -> Self {
        let mut main_tab = TabEntry::new(main_session_id, "default");
        main_tab.is_main = true;
        Self {
            tabs: vec![main_tab],
            active: 0,
            page_start: 0,
            last_term_width: 0,
            last_tab_area_x: 0,
            last_tab_area_y: 0,
            closed_tabs: Vec::new(),
            hidden_tabs: Vec::new(),
        }
    }

    /// 在 index=1 处插入新的子 agent tab。
    /// 若 active >= 1，自动将 active 加 1 以保持指向同一 tab。
    /// 返回新 tab 的索引。
    pub fn insert_sub_agent(
        &mut self,
        session_id: impl Into<String>,
        agent_name: impl Into<String>,
        view_only: bool,
    ) -> usize {
        let entry = if view_only {
            TabEntry::new_view_only(session_id, agent_name)
        } else {
            TabEntry::new(session_id, agent_name)
        };
        self.tabs.insert(1, entry);
        if !view_only {
            self.tabs[1].generating = true;
        }
        if self.active >= 1 {
            self.active += 1;
        }
        1
    }

    pub fn find_index_by_session(&self, session_id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.session_id == session_id)
    }

    pub fn find_or_insert(&mut self, session_id: &str, agent_name: &str) -> usize {
        // Default tab matches — return 0
        if session_id == self.tabs[0].session_id {
            return 0;
        }
        // Already exists
        if let Some(idx) = self.find_index_by_session(session_id) {
            return idx;
        }
        // Create new (default Running)
        self.insert_sub_agent(session_id, agent_name, false)
    }

    /// 计算每页的 sub 索引范围（不含 default）。
    /// 每页固定 4 个 sub tab。
    pub fn layout_pages(&self, _term_width: u16) -> Vec<std::ops::Range<usize>> {
        let sub_count = self.tabs.len().saturating_sub(1);
        if sub_count == 0 {
            #[allow(clippy::single_range_in_vec_init)]
            return vec![1..1]; // 空范围
        }
        const PER_PAGE: usize = 4;
        let total_pages = sub_count.div_ceil(PER_PAGE);
        (0..total_pages)
            .map(|p| {
                let start = 1 + p * PER_PAGE;
                let end = (start + PER_PAGE).min(self.tabs.len());
                start..end
            })
            .collect()
    }

    /// 当前页号（从 0 开始）
    pub fn current_page(&self) -> usize {
        self.page_start
    }

    /// 当前页的 sub 范围
    pub fn current_page_subs(&self, term_width: u16) -> std::ops::Range<usize> {
        let pages = self.layout_pages(term_width);
        if pages.is_empty() {
            return 0..0;
        }
        let p = self.page_start.min(pages.len() - 1);
        pages[p].clone()
    }

    /// active tab 在当前页可见 titles 中的索引。
    /// 仅当 active 在当前页可见时返回 Some。
    pub fn select_idx_for_current_page(&self, term_width: u16) -> Option<usize> {
        if self.active == 0 {
            return Some(0);
        }
        let range = self.current_page_subs(term_width);
        if range.contains(&self.active) {
            // visible titles = [default, sub_range[0], sub_range[1], ...]
            Some(1 + (self.active - range.start))
        } else {
            None
        }
    }

    /// 翻到下一页（边界停止）。
    pub fn next_page(&mut self, term_width: u16) -> bool {
        let pages = self.layout_pages(term_width);
        if self.page_start + 1 < pages.len() {
            self.page_start += 1;
            true
        } else {
            false
        }
    }

    /// 翻到上一页（边界停止）。
    pub fn prev_page(&mut self) -> bool {
        if self.page_start > 0 {
            self.page_start -= 1;
            true
        } else {
            false
        }
    }

    /// 确保 active tab 在当前页可见。
    /// 如果 active 不在可见范围，翻到包含它的那一页。
    pub fn ensure_active_visible(&mut self, term_width: u16) {
        if self.active == 0 {
            return; // default 永远可见
        }
        let pages = self.layout_pages(term_width);
        for (i, range) in pages.iter().enumerate() {
            if range.contains(&self.active) {
                self.page_start = i;
                return;
            }
        }
    }

    /// Activate the tab at the given index and render its pending frames.
    pub fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.tabs[index].render_pending();
    }

    /// Activate the next tab (circular). No-op if empty.
    pub fn activate_next(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let next = (self.active + 1) % self.tabs.len();
        self.activate(next);
    }

    /// Activate the previous tab (circular). No-op if empty.
    pub fn activate_prev(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let prev = if self.active == 0 {
            self.tabs.len() - 1
        } else {
            self.active - 1
        };
        self.activate(prev);
    }

    /// Close the active sub-agent tab.
    /// Only allowed when active > 0 (sub tab).
    /// The tab is moved to closed_tabs for later restoration.
    /// Returns true if the tab was closed; false otherwise.
    pub fn close_active(&mut self) -> bool {
        if self.active == 0 {
            return false;
        }
        let removed = self.tabs.remove(self.active);
        self.closed_tabs.push(removed);
        if self.active > 0 {
            self.active -= 1;
        }
        self.tabs[self.active].render_pending();
        self.ensure_active_visible(self.last_term_width);
        true
    }

    /// Close a specific sub-agent tab by index.
    /// Only allowed for sub tabs (index > 0).
    /// The tab is moved to closed_tabs for later restoration.
    /// Returns true if the tab was closed; false otherwise.
    /// Note: closing a Running tab does not cancel the agent; the agent
    /// continues in the background and its output is simply not displayed.
    pub fn close_tab(&mut self, idx: usize) -> bool {
        if idx == 0 || idx >= self.tabs.len() {
            return false;
        }
        let removed = self.tabs.remove(idx);
        self.closed_tabs.push(removed);
        if self.active >= idx && self.active > 0 {
            self.active -= 1;
        }
        self.tabs[self.active].render_pending();
        self.ensure_active_visible(self.last_term_width);
        true
    }

    /// 重连恢复：丢弃全部 subagent tab（活跃与 hidden），**不写入回收站**。
    ///
    /// subagent 历史为纯内存态、不可恢复；写入 `closed_tabs` 会让用户经
    /// AgentCall 按钮恢复出一个空 tab。主 tab（index 0）保留。
    pub fn discard_subagent_tabs(&mut self) {
        self.tabs.truncate(1);
        self.hidden_tabs.clear();
        self.active = 0;
        self.page_start = 0;
    }

    /// 查找或恢复子 agent tab。
    /// 如果活跃 tabs 中存在则返回索引；否则从 closed_tabs 或 hidden_tabs 恢复。
    /// 返回 Some(idx) 表示找到或恢复成功。
    pub fn find_or_restore_tab(&mut self, session_id: &str) -> Option<usize> {
        // 先在活跃 tabs 中查找
        if let Some(idx) = self.find_index_by_session(session_id) {
            return Some(idx);
        }
        // 从 closed_tabs 恢复
        if let Some(pos) = self
            .closed_tabs
            .iter()
            .position(|t| t.session_id == session_id)
        {
            let mut tab = self.closed_tabs.remove(pos);
            tab.render_pending();
            self.tabs.insert(1, tab);
            if self.active >= 1 {
                self.active += 1;
            }
            return Some(1);
        }
        // 从 hidden_tabs 恢复
        if let Some(pos) = self
            .hidden_tabs
            .iter()
            .position(|t| t.session_id == session_id)
        {
            let mut tab = self.hidden_tabs.remove(pos);
            tab.generating = tab.status == AgentStatus::Running;
            tab.render_pending();
            self.tabs.insert(1, tab);
            if self.active >= 1 {
                self.active += 1;
            }
            return Some(1);
        }
        None
    }

    /// 按 session_id 查找 hidden_tab 的可变引用（用于路由帧到隐藏 tab）。
    /// 如果不存在则创建一个。
    fn find_or_create_hidden_tab(&mut self, session_id: &str, agent_name: &str) -> &mut TabEntry {
        let pos = self
            .hidden_tabs
            .iter()
            .position(|t| t.session_id == session_id);
        match pos {
            Some(i) => &mut self.hidden_tabs[i],
            None => {
                self.hidden_tabs.push(TabEntry::new(session_id, agent_name));
                self.hidden_tabs.last_mut().unwrap()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub query_id: String,
    pub message: String,
    pub options: Vec<String>,
    pub selected_index: usize,
    pub other_active: bool,
}

/// 拆分 assistant 消息内容与尾部统计行。
///
/// 统计行由 `consume_pending_usage` 以 `\n\n[...]` 追加到内容末尾（含
/// "| Tokens"），返回 `(正文, Some(统计行含方括号))`；无统计行时返回
/// 原内容与 None。
fn split_assistant_footer(content: &str) -> (&str, Option<&str>) {
    if let Some(idx) = content.rfind("\n\n[") {
        let footer = &content[idx + 2..];
        if footer.ends_with(']') && footer.contains("| Tokens") {
            return (&content[..idx], Some(footer));
        }
    }
    (content, None)
}

pub struct MessageCache {
    pub msg_id: u64,
    pub msg_version: u64,
    pub width: u16,
    pub expanded: bool,
    pub lines: Vec<Line<'static>>,
    pub line_count: u16,
    pub image_state: Option<ImageState>, // None for non-image messages
}

impl MessageCache {
    pub fn from_message(
        msg: &ChatLine,
        width: u16,
        expanded: bool,
        image_metrics: Option<&ImageMetrics>,
    ) -> Self {
        // 背景色由 ui.rs 的 BlockStyle::bg_fill 统一处理
        let base_style = Style::default().fg(theme::fg_for(msg.line_type.clone()));
        let lines: Vec<Line<'static>> = match msg.line_type {
            LineType::Assistant => {
                // assistant 消息无边框渲染：markdown 内容占满整行宽度，内容尾部
                // 若带统计行（consume_pending_usage 追加的
                // `[time | Tokens ... | Tools: n]`）则作为页脚显示（与正文空一行）。
                let inner_w = (width as usize).max(1);
                let (body, footer) = split_assistant_footer(&msg.content);
                // 第一步：用 syntect 高亮代码块，替换为标记
                let (processed, highlighted_blocks) = process_code_blocks(body);
                // 第二步：ratatui-markdown 渲染（代码块位置是标记）
                use ratatui_markdown::markdown::MarkdownRenderer;
                let renderer = MarkdownRenderer::new(inner_w);
                let blocks = renderer.parse(&processed);
                let md_lines =
                    renderer.render(&blocks, &ratatui_markdown::theme::ThemeConfig::default());
                // 第三步：将标记替换为 syntect 高亮行
                let body_lines: Vec<Line<'static>> = md_lines
                    .into_iter()
                    .flat_map(|l| {
                        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                        if let Some(rest) = text.strip_prefix('\x00')
                            && let Some(id_tag) = rest.strip_prefix("CODEBLOCK_")
                            && let Some(end) = id_tag.find('\x00')
                            && let Ok(idx) = id_tag[..end].parse::<usize>()
                            && idx < highlighted_blocks.len()
                        {
                            // 代码行按内宽折行（保留语法高亮样式）。超宽的代码行
                            // 会被 render_block 的 Paragraph（无 Wrap）直接裁掉。
                            return highlighted_blocks[idx]
                                .iter()
                                .flat_map(|cl| wrap_styled_line(cl, inner_w))
                                .collect();
                        }
                        // 非代码行：白色
                        vec![Line::styled(text, Style::default().fg(theme::ASSISTANT_FG))]
                    })
                    .collect();
                // 第四步：页脚（正文与页脚之间空一行）
                let footer_fg = Style::default().fg(theme::TOOL_RESULT_FG);
                let mut out = body_lines;
                if let Some(footer) = footer {
                    out.push(Line::from(String::new()));
                    for fl in wrap_text(footer, inner_w as u16) {
                        out.push(Line::styled(fl, footer_fg));
                    }
                }
                out
            }
            LineType::Image {
                ref path,
                ref remote_url,
                ..
            } => {
                // Cache key: use path if non-empty, otherwise use remote_url
                let cache_key = if path.is_empty() {
                    remote_url.as_deref().unwrap_or("")
                } else {
                    path.as_str()
                };
                let (mut line_count, image_state) = match image_metrics {
                    Some(metrics) if !cache_key.is_empty() => {
                        match metrics
                            .image_cache
                            .query_height(cache_key, width, metrics.max_rows)
                        {
                            ImageHeightInfo::Ready(h) => (h, Some(ImageState::Ready)),
                            ImageHeightInfo::Placeholder => {
                                // Check if it's Loading or Error
                                let state = metrics.image_cache.image_state(cache_key);
                                match state {
                                    Some(ImageState::Loading) => (1, Some(ImageState::Loading)),
                                    _ => (1, Some(ImageState::Error)),
                                }
                            }
                        }
                    }
                    _ => (1, None),
                };
                // Add address line count (wrapped at `width`), matching render_image_block
                if let Some(url) = remote_url.as_ref().filter(|u| !u.is_empty()) {
                    line_count += wrap_text(&format!("🔗 {}", url), width).len() as u16;
                }
                if !path.is_empty() {
                    line_count += wrap_text(&format!("📁 {}", path), width).len() as u16;
                }
                // For image lines, lines vec is empty (rendered separately by render_image_block)
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines: Vec::new(),
                    line_count,
                    image_state,
                };
            }
            LineType::ToolCall { .. } => {
                let lines = crate::tool_ui::render_tool_call(msg, width, expanded);
                let line_count = lines.len() as u16;
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines,
                    line_count,
                    image_state: None,
                };
            }
            LineType::AgentCall { .. } => {
                let lines = crate::tool_ui::render_agent_call(msg, width, expanded);
                let line_count = lines.len() as u16;
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines,
                    line_count,
                    image_state: None,
                };
            }
            LineType::ToolResult { .. } => {
                let lines = crate::tool_ui::render_tool_result(msg, width);
                let line_count = lines.len() as u16;
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines,
                    line_count,
                    image_state: None,
                };
            }
            LineType::ToolError { .. } => {
                let lines = crate::tool_ui::render_tool_error(msg, width);
                let line_count = lines.len() as u16;
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines,
                    line_count,
                    image_state: None,
                };
            }
            _ => {
                let mut lines = Vec::new();
                let wrapped = wrap_text(&msg.content, width);
                for dl in wrapped.iter() {
                    let content = if dl.is_empty() {
                        " ".repeat(width as usize)
                    } else {
                        pad_to_width(dl, width as usize)
                    };
                    lines.push(Line::styled(content, base_style));
                }
                let line_count = lines.len() as u16;
                return Self {
                    msg_id: msg.id,
                    msg_version: msg.version,
                    width,
                    expanded,
                    lines,
                    line_count,
                    image_state: None,
                };
            }
        };

        let line_count = lines.len() as u16;
        Self {
            msg_id: msg.id,
            msg_version: msg.version,
            width,
            expanded,
            lines,
            line_count,
            image_state: None,
        }
    }

    pub fn matches(&self, msg: &ChatLine, width: u16, expanded: bool) -> bool {
        self.msg_id == msg.id
            && self.msg_version == msg.version
            && self.width == width
            && self.expanded == expanded
    }
}

pub(crate) fn wrap_text(text: &str, screen_width: u16) -> Vec<String> {
    let mut result = Vec::new();
    let sw = screen_width as usize;
    if sw == 0 {
        return result;
    }
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            result.push(String::new());
            continue;
        }
        let chars: Vec<char> = paragraph.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            // 寻找可折行位置：优先按单词边界（空白字符）断行
            let mut w: usize = 0;
            let mut end = start;
            let mut break_at = None; // 最后一个空白符位置

            for (i, &c) in chars[start..].iter().enumerate() {
                let cw = c.width().unwrap_or(0);

                // 记录空白符位置作为可选断点
                if c.is_whitespace() && w > 0 {
                    break_at = Some((start + i, w));
                }

                if w + cw > sw {
                    if let Some((bp, _)) = break_at {
                        // 有空白符可断：在空白符处折行
                        end = bp;
                        break;
                    }
                    // 整个单词超过一行宽度，允许字符断行
                    if w > 0 {
                        end = start + i;
                        break;
                    }
                }

                w += cw;
                end = start + i + 1;

                // 不主动在 w == sw 时断行，留给下一轮 overflow 逻辑
                // 处理，确保在单词边界折行，而非截断单词
            }

            result.push(chars[start..end].iter().collect());
            start = end;

            // 如果断点在空白符，跳过后续空白避免空行
            while start < chars.len() && chars[start].is_whitespace() {
                start += 1;
            }
        }
    }
    result
}

pub(crate) fn pad_to_width(s: &str, width: usize) -> String {
    let len: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if len < width {
        format!("{}{}", s, " ".repeat(width - len))
    } else {
        s.to_string()
    }
}

/// Session 选择器状态（ratatui List widget）
pub struct SessionSelectState {
    pub labels: Vec<String>,
    pub state: ListState,
    pub session_ids: Vec<String>,
}

/// 模型选择器状态
pub struct ModelSelectState {
    /// 选择器中显示的标签（如 "GPT-4o (OpenAI)"）
    pub display_labels: Vec<String>,
    /// 发送给 daemon 的模型名称（如 "GPT-4o"）
    pub model_keys: Vec<String>,
    pub state: ListState,
}

/// 把结构化验证事件渲染为若干 ChatLine 文本（UI 层不解析模型输出，只展示结构化字段）。
pub(crate) fn format_verification_lines(v: &visp_proto::visp::VerificationUpdate) -> Vec<String> {
    let head = match v.stage.as_str() {
        "started" => format!(
            "🔍 独立验证：从原始需求出发验证改动（第 {} 轮，返工上限 {}）",
            v.round, v.max_rounds
        ),
        "passed" => format!("✅ 验证通过：{}", v.summary),
        "failed" => {
            if v.is_final {
                format!("❌ 验证未通过：{}", v.summary)
            } else {
                format!("❌ 验证未通过（将继续返工）：{}", v.summary)
            }
        }
        "rework" => format!("↩ {}", v.summary),
        "exhausted" => format!(
            "❌ 验证未通过（已用尽 {} 轮返工）：{}",
            v.max_rounds, v.summary
        ),
        "inconclusive" => format!("⚠️ 验证未完成（验证没做成，不是发现缺陷）：{}", v.summary),
        other => format!("[验证 {}] {}", other, v.summary),
    };
    let mut lines = vec![head];
    if !v.evidence.is_empty() {
        lines.push("证据：".to_string());
        for ev in &v.evidence {
            lines.push(format!(
                "  - 检查：{}；方法：{}；结果：{}",
                ev.check, ev.method, ev.result
            ));
        }
    }
    lines
}

/// 从 ServerMessage 中提取 (session_id, agent_name)。
/// 没有 agent_name 字段的 payload 返回空字符串。
fn extract_session_and_agent(msg: &ServerMessage) -> (String, String) {
    match &msg.payload {
        Some(server_message::Payload::TextDelta(d)) => (d.session_id.clone(), d.agent_name.clone()),
        Some(server_message::Payload::ToolCall(d)) => (d.session_id.clone(), d.agent_name.clone()),
        Some(server_message::Payload::ToolResult(d)) => {
            (d.session_id.clone(), d.agent_name.clone())
        }
        Some(server_message::Payload::StatusUpdate(d)) => {
            (d.session_id.clone(), d.agent_name.clone())
        }
        Some(server_message::Payload::Error(d)) => (d.session_id.clone(), d.agent_name.clone()),
        Some(server_message::Payload::Done(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::UserQuery(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::ThinkingBlock(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::UsageInfo(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::UsageDelta(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::UserMessage(d)) => (d.session_id.clone(), String::new()),
        Some(server_message::Payload::ImageBlock(d)) => {
            (d.session_id.clone(), d.agent_name.clone())
        }
        Some(server_message::Payload::ImageError(d)) => {
            (d.session_id.clone(), d.agent_name.clone())
        }
        Some(server_message::Payload::VerificationUpdate(d)) => {
            (d.session_id.clone(), d.agent_name.clone())
        }
        None => (String::new(), String::new()),
    }
}

pub struct AppState {
    pub tab_bar: TabBar,
    pub main_session_id: String,
    /// 当前请求的 token 用量 (input, output, cache_create, cache_read)
    pub current_request_usage: (u32, u32, u32, u32),
    pub message_caches: Vec<MessageCache>,
    pub scroll_following: bool,
    pub scroll_state: ScrollState,
    /// 滚动条几何（渲染时更新，供滚动条拖拽命中测试）
    pub scrollbar_geo: Option<ScrollbarGeo>,
    /// 滚动条拖拽中：指针相对滑块顶部的偏移（None = 未拖拽）
    pub scrollbar_drag_offset: Option<u16>,
    pub cache_width: u16,

    // 输入
    pub textarea: ratatui_textarea::TextArea<'static>,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,

    // 状态
    /// generating 期间的 spinner 帧序号，由主循环 tick 递增
    pub spinner_frame: usize,
    pub stale_done_expected: bool,
    /// 当前正在处理的请求 ID（用于 Done 后发 Ack）
    pub current_request_id: Option<String>,
    pub needs_render: bool,
    pub last_scroll_time: Option<std::time::Instant>,
    pub last_stream_render: Option<std::time::Instant>,
    pub confirm: Option<ConfirmState>,
    pub model: String,
    pub model_key: String,
    pub session_id: String,
    pub should_quit: bool,
    /// 连接状态机当前状态（设计 §5.9，逻辑集中在 event loop）。
    /// 重连期间用于输入保护（Enter 提示不发送）与状态提示。
    pub connection_state: ConnState,
    /// 当前 session 累计 token 数（input + output），用于状态栏显示
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub total_cache_creation_input_tokens: u32,
    pub total_cache_read_input_tokens: u32,
    /// 当前 session 累计成本（provider 上报；0 表示未提供，不显示）
    pub total_cost: f64,
    /// 鼠标文本选择状态（内容坐标）
    pub text_selection: crate::selection::TextSelection,
    /// 上一次复制是否成功（用于状态提示）
    pub last_copy_msg: Option<String>,
    /// 标记：下一帧渲染时从 buffer 提取选中文字
    pub pending_copy: bool,
    /// 渲染时提取的待复制文本（draw 完成后由主循环执行 OSC 52）
    pub pending_copy_text: Option<String>,
    /// last_copy_msg 的设置时间，用于自动清除提示
    pub last_copy_time: Option<std::time::Instant>,
    /// 上一次鼠标左键按下的时间（用于双击检测）
    pub last_mouse_click: Option<std::time::Instant>,
    /// 上一次渲染的 chat area 矩形（屏幕坐标，用于鼠标命中测试）
    pub chat_area_rect: (u16, u16, u16, u16),
    /// 状态栏第 1 行的屏幕起点 (x, y)，用于 session_id 单击复制命中判定
    pub status_bar_first_row: Option<(u16, u16)>,
    /// 拖拽选择高亮用：渲染后每个屏幕行的 block 归属。
    /// 每次渲染前清空，由 render_block / render_image_block 填充。
    pub block_rows: BlockRows,
    /// 用户输入了 /new 命令，主循环需要创建新 session
    pub pending_new_session: bool,
    /// 用户输入了 /list 命令，主循环需要列出 session
    pub pending_list_sessions: bool,
    /// 用户输入了 /sessions <id>，主循环需要切换到目标 session
    pub pending_switch_session: Option<String>,
    /// 用户输入了 /reload，主循环需要调用 ReloadConfig unary RPC
    pub pending_reload: bool,
    /// 是否显示帮助弹窗
    pub show_help: bool,
    /// session 选择器弹出面板（/list 或 /sessions 无参触发）
    pub session_select: Option<SessionSelectState>,
    /// 可用的模型名称列表（显示标签）
    pub available_models: Vec<String>,
    /// 可用的模型 lookup key 列表
    pub model_keys: Vec<String>,
    /// 终端通知引擎（由 `event::run` 注入；`AppState::new` 时为 disabled）
    pub notify: NotifyEngine,
    /// 用户输入了 /model（无参），主循环需要获取模型列表并显示选择器
    pub pending_model_select: bool,
    /// 模型选择器弹出面板（/model 无参触发）
    pub model_select: Option<ModelSelectState>,
    /// 项目路径，用于文件操作
    pub project_path: String,
    /// Tab 补全状态：记录原始前缀和匹配列表，用于循环切换
    pub tab_completion: Option<TabCompletionState>,
    /// 已展开的工具调用 call_id 集合（点击切换）
    pub expanded_tool_calls: std::collections::HashSet<String>,
    /// 图片缓存（终端图片渲染）
    pub image_cache: crate::image::ImageCache,
    /// 网络图片下载完成通知 channel（主循环监听）
    pub image_ready_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

/// Tab 补全循环状态
#[derive(Debug, Clone)]
pub struct TabCompletionState {
    /// 用户输入的原始前缀（如 "/i"）
    pub prefix: String,
    /// 所有匹配的命令列表
    pub matches: Vec<String>,
    /// 当前选中的索引
    pub index: usize,
}

impl AppState {
    pub fn new(session_id: String, model: String, model_key: String, project_path: String) -> Self {
        let mut textarea = Self::new_textarea();
        textarea.set_placeholder_text("Type your message...");
        let main_session_id = session_id.clone();
        let (image_ready_tx, image_ready_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        Self {
            tab_bar: TabBar::new(main_session_id.clone()),
            main_session_id,
            current_request_usage: (0, 0, 0, 0),
            message_caches: Vec::new(),
            scroll_following: true,
            scroll_state: ScrollState::default(),
            scrollbar_geo: None,
            scrollbar_drag_offset: None,
            cache_width: 0,
            textarea,
            input_history: Vec::new(),
            history_index: None,
            spinner_frame: 0,
            stale_done_expected: false,
            current_request_id: None,
            needs_render: true,
            last_scroll_time: None,
            last_stream_render: None,
            confirm: None,
            model,
            model_key,
            session_id,
            should_quit: false,
            connection_state: ConnState::Connected,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_cache_read_input_tokens: 0,
            total_cost: 0.0,
            text_selection: crate::selection::TextSelection::default(),
            last_copy_msg: None,
            pending_copy: false,
            pending_copy_text: None,
            last_copy_time: None,
            last_mouse_click: None,
            chat_area_rect: (0, 0, 0, 0),
            status_bar_first_row: None,
            block_rows: BlockRows::default(),
            pending_new_session: false,
            pending_list_sessions: false,
            pending_switch_session: None,
            pending_reload: false,
            show_help: false,
            session_select: None,
            available_models: Vec::new(),
            model_keys: Vec::new(),
            notify: NotifyEngine::disabled(),
            pending_model_select: false,
            model_select: None,
            project_path,
            tab_completion: None,
            expanded_tool_calls: std::collections::HashSet::new(),
            image_cache: {
                let mut cache = crate::image::ImageCache::new();
                cache.set_ready_tx(image_ready_tx);
                cache
            },
            image_ready_rx,
        }
    }

    /// 创建配置了 word wrap 的新 TextArea
    pub fn new_textarea() -> ratatui_textarea::TextArea<'static> {
        let mut ta = ratatui_textarea::TextArea::default();
        ta.set_wrap_mode(WrapMode::WordOrGlyph);
        ta.set_placeholder_text("Type your message...");
        ta
    }

    /// 切换工具调用块的展开/折叠状态
    pub fn toggle_tool_call_expansion(&mut self, call_id: &str) {
        if self.expanded_tool_calls.contains(call_id) {
            self.expanded_tool_calls.remove(call_id);
        } else {
            self.expanded_tool_calls.insert(call_id.to_string());
        }
    }

    // ── Tab 访问方法 ────────────────────────────────────────
    pub fn active_tab(&self) -> &TabEntry {
        &self.tab_bar.tabs[self.tab_bar.active]
    }

    pub fn active_tab_mut(&mut self) -> &mut TabEntry {
        let idx = self.tab_bar.active;
        &mut self.tab_bar.tabs[idx]
    }

    pub fn active_messages(&self) -> &[ChatLine] {
        &self.active_tab().messages
    }

    // ── 消息操作 ────────────────────────────────────────────
    //
    // 设计约定（Step 5）：
    // - 类型 A（本地 UI 反馈/用户输入）：永远写 default tab（tabs[0]）
    // - 类型 B（agent 事件）：按 session_id 路由到对应 tab

    /// 类型 A：永远写 default tab（tabs[0]）
    pub fn add_message(&mut self, line_type: LineType, content: String) {
        // 包含 <image: ...> 标记时拆分为多条 ChatLine（文本 + 图片）
        let lines = crate::image::split_image_markers(&content, line_type);
        self.tab_bar.tabs[0].push_chat_lines(lines);
    }

    /// 类型 B：按 session_id 路由
    pub fn add_message_to_session(
        &mut self,
        session_id: &str,
        line_type: LineType,
        content: String,
    ) {
        // 包含 <image: ...> 标记时拆分为多条 ChatLine（文本 + 图片）
        let lines = crate::image::split_image_markers(&content, line_type);
        self.tab_mut_by_session(session_id).push_chat_lines(lines);
    }

    /// 类型 B：按 session_id 路由的 tool_line
    pub fn add_tool_line_to_session(
        &mut self,
        session_id: &str,
        line_type: LineType,
        content: String,
        call_id: &str,
    ) {
        self.tab_mut_by_session(session_id).push_chat_line(
            line_type,
            content,
            Some(call_id.to_string()),
        );
    }

    /// 类型 B：按 session_id 路由的 update_thinking
    pub fn update_thinking_to_session(&mut self, session_id: &str, content: String) {
        self.tab_mut_by_session(session_id).update_thinking(content);
    }

    /// 类型 B：按 session_id 路由的 append_streaming
    pub fn append_streaming_to_session(&mut self, session_id: &str, delta: &str) {
        self.tab_mut_by_session(session_id)
            .streaming_text
            .push_str(delta);
    }

    /// 类型 B：按 session_id 路由的 flush_streaming
    pub fn flush_streaming_to_session(&mut self, session_id: &str) {
        self.tab_mut_by_session(session_id).flush_streaming();
    }

    /// 内部：按 session_id 找 tab，未知 ID 回退到 default（tabs[0]）
    fn tab_mut_by_session(&mut self, session_id: &str) -> &mut TabEntry {
        let idx = self.tab_bar.find_index_by_session(session_id).unwrap_or(0);
        &mut self.tab_bar.tabs[idx]
    }

    // ── 兼容 shim（旧签名，路由到 main_session_id）─────────────

    /// 兼容 shim：路由到 main_session_id 对应 tab
    pub fn add_tool_line(&mut self, line_type: LineType, content: String, call_id: &str) {
        let main_sid = self.main_session_id.clone();
        self.add_tool_line_to_session(&main_sid, line_type, content, call_id);
    }

    /// 兼容 shim：路由到 main_session_id 对应 tab
    pub fn append_streaming(&mut self, delta: &str) {
        let main_sid = self.main_session_id.clone();
        self.append_streaming_to_session(&main_sid, delta);
    }

    /// 兼容 shim：路由到 main_session_id 对应 tab
    pub fn flush_streaming(&mut self) {
        let main_sid = self.main_session_id.clone();
        self.flush_streaming_to_session(&main_sid);
    }

    /// 兼容 shim：路由到 main_session_id 对应 tab
    pub fn update_thinking(&mut self, content: String) {
        let main_sid = self.main_session_id.clone();
        self.update_thinking_to_session(&main_sid, content);
    }

    // ── 保留（非 session 路由，仍操作 active tab）──────────────

    pub fn insert_tool_result(&mut self, call_id: &str, content: String) {
        self.active_tab_mut().insert_tool_result(call_id, content);
    }

    pub fn update_message(&mut self, id: u64, content: String) {
        self.active_tab_mut().update_message(id, content);
    }

    // ── 其他原有方法 ─────────────────────────────────────────

    pub fn try_begin_scroll(&mut self) -> bool {
        const COOLDOWN_MS: u128 = 30;
        let now = std::time::Instant::now();
        if let Some(last) = self.last_scroll_time
            && now.duration_since(last).as_millis() < COOLDOWN_MS
        {
            return false;
        }
        self.last_scroll_time = Some(now);
        true
    }

    /// Points 三点跑马灯当前帧（generating 期间用作 placeholder 动画）
    pub fn spinner_glyph(&self) -> &'static str {
        const FRAMES: [&str; 4] = [
            "\u{2219}\u{2219}\u{2219}",
            "\u{25cf}\u{2219}\u{2219}",
            "\u{2219}\u{25cf}\u{2219}",
            "\u{2219}\u{2219}\u{25cf}",
        ];
        FRAMES[self.spinner_frame % FRAMES.len()]
    }

    pub fn try_begin_stream_render(&mut self) -> bool {
        const COOLDOWN_MS: u128 = 30;
        let now = std::time::Instant::now();
        if let Some(last) = self.last_stream_render
            && now.duration_since(last).as_millis() < COOLDOWN_MS
        {
            return false;
        }
        self.last_stream_render = Some(now);
        true
    }

    // ════════════════════════════════════════════════════════════
    // Shim API — 代理到 active_tab() / active_tab_mut()
    // ════════════════════════════════════════════════════════════

    // --- 读取（getter）---
    pub fn messages(&self) -> &[ChatLine] {
        &self.active_tab().messages
    }
    pub fn streaming_text(&self) -> &str {
        &self.active_tab().streaming_text
    }
    pub fn streaming_is_empty(&self) -> bool {
        self.active_tab().streaming_text.is_empty()
    }
    pub fn streaming_lines_count(&self) -> usize {
        self.active_tab().streaming_text.lines().count()
    }
    pub fn generating(&self) -> bool {
        self.active_tab().generating
    }
    pub fn has_pending_usage(&self) -> bool {
        self.active_tab().pending_usage.is_some()
    }

    // --- 写入（操作型）---
    pub fn set_generating(&mut self, v: bool) {
        let tab = self.active_tab_mut();
        tab.generating = v;
        // 重新开始生成或停止生成都重置流式计时器（提交新请求 / 取消 / 会话切换），
        // 并清除冻结速率与冻结耗时，避免上一轮的值泄漏进本轮统计行
        tab.stream_started_at = None;
        tab.stream_output_tokens = 0;
        tab.last_stream_tps = None;
        tab.last_stream_elapsed = None;
    }
    pub fn clear_streaming(&mut self) {
        self.active_tab_mut().streaming_text.clear();
    }
    pub fn append_streaming_text(&mut self, s: &str) {
        self.active_tab_mut().streaming_text.push_str(s);
    }
    pub fn set_pending_usage(&mut self, v: Option<(u32, u32, u32, u32, u32)>) {
        self.active_tab_mut().pending_usage = v;
    }
    pub fn take_pending_usage(&mut self) -> Option<(u32, u32, u32, u32, u32)> {
        self.active_tab_mut().pending_usage.take()
    }
    pub fn clear_pending_usage(&mut self) {
        self.active_tab_mut().pending_usage = None;
    }

    // --- 用于 ui.rs 的迭代访问（消息存在性查询）---
    pub fn has_message_with_id(&self, id: u64) -> bool {
        self.active_tab().messages.iter().any(|m| m.id == id)
    }

    pub fn clear_messages(&mut self) {
        self.active_tab_mut().messages.clear();
        self.message_caches.clear();
    }

    /// 尝试将主 tab 中匹配的 ToolCall 转为 AgentCall，并关联 sub_session_id。
    /// 匹配条件：tool_name == agent_name 且尚未关联 session。
    /// 幂等：已升级的 AgentCall 不会被重复处理（sub_session_id 已设置）。
    fn try_upgrade_toolcall_to_agentcall(&mut self, agent_name: &str, session_id: &str) {
        if let Some(main_msg) = self.tab_bar.tabs[0].messages.iter_mut().find(|m| {
            matches!(&m.line_type, LineType::ToolCall { name } if name == agent_name)
                && m.sub_session_id.is_none()
        }) {
            main_msg.sub_session_id = Some(session_id.to_string());
            let name = match &main_msg.line_type {
                LineType::ToolCall { name } => name.clone(),
                _ => agent_name.to_string(),
            };
            main_msg.line_type = LineType::AgentCall { name };
            main_msg.version += 1;
        }
    }

    /// 从主 tab 中查找尚未关联 session 的 ToolCall，提取 prompt。
    fn extract_prompt_for_sub_agent(&self, agent_name: &str) -> Option<String> {
        self.tab_bar.tabs[0].messages.iter().find_map(|m| {
            if matches!(&m.line_type, LineType::ToolCall { name } if name == agent_name)
                && m.tool_result.is_none()
                && m.sub_session_id.is_none()
            {
                serde_json::from_str::<serde_json::Value>(&m.content)
                    .ok()
                    .and_then(|v| v.get("prompt").and_then(|p| p.as_str()).map(String::from))
            } else {
                None
            }
        })
    }

    /// 根据 session_id 将 ServerMessage 路由到正确的 tab，
    /// 如果是 active tab 则立即调用 render_pending()。
    /// 子 agent 帧默认路由到 hidden_tabs（不自动创建活跃 tab），
    /// 仅 ViewOnly 帧和用户点击按钮时才创建活跃 tab。
    pub fn route_frame(&mut self, frame: ServerMessage) {
        let (session_id, agent_name) = extract_session_and_agent(&frame);

        let idx = if session_id.is_empty() || session_id == self.main_session_id {
            0
        } else if let Some(i) = self.tab_bar.find_index_by_session(&session_id) {
            // 活跃 tabs 中已有 -> 路由到此
            if !agent_name.is_empty() && self.tab_bar.tabs[i].agent_name == "agent" {
                self.tab_bar.tabs[i].agent_name = agent_name.clone();
            }
            if !agent_name.is_empty()
                && self.tab_bar.tabs[i].task_prompt.is_none()
                && let Some(prompt) = self.extract_prompt_for_sub_agent(&agent_name)
            {
                self.tab_bar.tabs[i].task_prompt = Some(prompt);
            }
            if !agent_name.is_empty() {
                self.try_upgrade_toolcall_to_agentcall(&agent_name, &session_id);
            }
            i
        } else {
            // 子 agent 帧路由到 hidden_tab（不自动创建活跃 tab）
            let view_only = matches!(&frame.payload,
                Some(server_message::Payload::StatusUpdate(su)) if su.view_only);
            let title = if agent_name.is_empty() {
                "agent".to_string()
            } else {
                agent_name.clone()
            };
            // ViewOnly 帧优先取 input_history[0] 作为 task_prompt（与历史行为一致）；
            // 否则从主 tab 中已升级的 ToolCall 提取 prompt
            let prompt = if view_only && !self.input_history.is_empty() {
                Some(self.input_history[0].clone())
            } else if !agent_name.is_empty() {
                self.extract_prompt_for_sub_agent(&agent_name)
            } else {
                None
            };
            let tab = self.tab_bar.find_or_create_hidden_tab(&session_id, &title);
            if view_only {
                tab.status = AgentStatus::ViewOnly;
            }
            if !agent_name.is_empty() && tab.agent_name == "agent" {
                tab.agent_name = agent_name.clone();
            }
            if let Some(prompt) = prompt
                && tab.task_prompt.is_none()
            {
                tab.task_prompt = Some(prompt);
            }
            // 状态更新
            match &frame.payload {
                Some(server_message::Payload::Done(_)) => {
                    if tab.status == AgentStatus::Running {
                        tab.status = AgentStatus::Done;
                    }
                }
                Some(server_message::Payload::Error(err)) => {
                    if err.code == "SessionBusy" {
                        // 忙拒绝不代表本轮失败：不置 Error，仅追加一行提示
                        tab.push_status_line_dedup(err.message.clone());
                    } else {
                        tab.status = AgentStatus::Error;
                    }
                }
                _ => {}
            }
            // 路由帧
            tab.frames.push(frame);
            if !agent_name.is_empty() {
                self.try_upgrade_toolcall_to_agentcall(&agent_name, &session_id);
            }
            return;
        };

        let active = self.tab_bar.active;

        // 立即根据 payload 更新 tab.status
        match &frame.payload {
            Some(server_message::Payload::Done(_)) => {
                if self.tab_bar.tabs[idx].status == AgentStatus::Running {
                    self.tab_bar.tabs[idx].status = AgentStatus::Done;
                }
            }
            Some(server_message::Payload::Error(err)) => {
                if err.code == "SessionBusy" {
                    // 忙拒绝不代表本轮失败：不置 Error，仅追加一行提示
                    // （去重，避免随后的 render_pending 针对同一帧再追加）
                    self.tab_bar.tabs[idx].push_status_line_dedup(err.message.clone());
                } else {
                    self.tab_bar.tabs[idx].status = AgentStatus::Error;
                    if err.code == "SessionNotActive" {
                        self.tab_bar.tabs[idx].push_chat_line(
                            LineType::Status,
                            "该会话已结束，无法继续输入".into(),
                            None,
                        );
                    }
                }
            }
            _ => {}
        }

        self.tab_bar.tabs[idx].frames.push(frame);

        if idx == active {
            self.tab_bar.tabs[idx].render_pending();
            // 流式内容（thinking、text delta 等）增长时，确保视图自动跟随到底部
            self.scroll_following = true;
        }
    }

    /// Token 三层路由 — L1: tab.pending_usage, L2: current_request_usage, L3: total_*_tokens
    ///
    /// 按 session_id 路由 UsageInfo 到对应 tab 的 pending_usage（L1）。
    /// 所有 agent（default + sub）的 token 都同时累加到 L2 与 L3：
    /// - L2: current_request_usage，主 session Done 时由 apply_done_token_settlement 清零
    /// - L3: total_*_tokens，立即累加，状态栏即时显示
    pub fn apply_usage_info(
        &mut self,
        session_id: &str,
        input: u32,
        output: u32,
        tool_calls: u32,
        cache_create: u32,
        cache_read: u32,
    ) {
        let is_main = session_id.is_empty() || session_id == self.main_session_id;

        // L1: 按 session_id 路由：空 ID 或主 session -> default tab
        // 子 agent 的 UsageInfo 路由到活跃 tab 或 hidden_tab，不自动创建活跃 tab
        if is_main {
            self.tab_bar.tabs[0].pending_usage =
                Some((input, output, tool_calls, cache_create, cache_read));
        } else if let Some(i) = self.tab_bar.find_index_by_session(session_id) {
            self.tab_bar.tabs[i].pending_usage =
                Some((input, output, tool_calls, cache_create, cache_read));
        } else {
            // 路由到 hidden_tab
            let tab = self.tab_bar.find_or_create_hidden_tab(session_id, "agent");
            tab.pending_usage = Some((input, output, tool_calls, cache_create, cache_read));
        }

        // L2: 累加到 current_request_usage（sub agent 的 token 同样汇聚到"本次请求"）
        self.current_request_usage.0 += input;
        self.current_request_usage.1 += output;
        self.current_request_usage.2 += cache_create;
        self.current_request_usage.3 += cache_read;
        // L3: 立即累加到 total tokens，状态栏即时显示
        self.total_input_tokens += input;
        self.total_output_tokens += output;
        self.total_cache_creation_input_tokens += cache_create;
        self.total_cache_read_input_tokens += cache_read;
    }

    /// 消费 provider 逐事件上报的增量 usage（`Payload::UsageDelta`）。
    ///
    /// 仅用于「生成中」的实时速率展示（累加到 tab 的 `stream_output_tokens`），
    /// **不会**累加进任何会话/请求总量——结算数据一律以 `UsageInfo` 为准，
    /// 在此处累加总量会导致重复计数。
    ///
    /// 路由与 [`AppState::apply_usage_info`] 的 L1 一致：空 ID/主 session 归
    /// default tab，命中已知 session 归对应 tab，否则归 hidden tab。
    pub fn apply_usage_delta(&mut self, session_id: &str, output_tokens: u32) {
        let is_main = session_id.is_empty() || session_id == self.main_session_id;
        let target = if is_main {
            &mut self.tab_bar.tabs[0]
        } else if let Some(i) = self.tab_bar.find_index_by_session(session_id) {
            &mut self.tab_bar.tabs[i]
        } else {
            self.tab_bar.find_or_create_hidden_tab(session_id, "agent")
        };
        target.stream_output_tokens = target
            .stream_output_tokens
            .saturating_add(u64::from(output_tokens));
        // 首个增量到达即启动速率时钟：纯 thinking / 工具调用流可能全程
        // 没有 TextDelta，若无时钟 tokens_per_second 会一直返回 None
        if target.stream_started_at.is_none() {
            target.stream_started_at = Some(std::time::Instant::now());
        }
    }

    /// 累计 provider 上报的成本（L3，状态栏展示）。
    ///
    /// `cost` 为本次 LLM 调用的成本；provider 未提供时不调用本方法（传入 0 亦无副作用）。
    /// 与 token 一致，所有 agent（default + sub）的成本都汇聚到会话总计。
    pub fn apply_usage_cost(&mut self, _session_id: &str, cost: f64) {
        if cost > 0.0 {
            self.total_cost += cost;
        }
    }

    /// Token 三层路由 — Done 结算
    ///
    /// - default tab（idx == 0）：清零 L2（L3 已在 apply_usage_info 中即时累加）
    /// - sub tab（idx != 0）：pending_usage 由 render_pending 消费，此处不做处理
    /// - 状态守卫：仅 Running 状态的 tab 执行结算；Error/Done 跳過
    pub fn apply_done_token_settlement(&mut self, session_id: &str) {
        let idx = if session_id.is_empty() || session_id == self.main_session_id {
            0
        } else if let Some(i) = self.tab_bar.find_index_by_session(session_id) {
            i
        } else {
            return; // Unknown session, skip
        };

        // 状态守卫：仅 Running 状态的 tab 执行 token 结算
        if self.tab_bar.tabs[idx].status != AgentStatus::Running {
            return;
        }

        if idx == 0 {
            // 清零 L2（L3 已在 apply_usage_info 中即时累加）
            self.current_request_usage = (0, 0, 0, 0);
        }
        // ── sub tab Done ──
        // pending_usage 由 TabEntry::render_pending 在 Done 帧处理时消费，
        // 此处不做任何处理
    }

    /// 重连成功后的多 tab 恢复清理（设计 §5.9）。
    ///
    /// - 主 tab 保留，其历史由随后 `send_join` 的权威回放重建；
    /// - subagent tab（活跃与 hidden）直接丢弃，**不写入 `closed_tabs` 回收站**
    ///   ——其历史纯内存态、不可恢复，入回收站会让用户经 AgentCall 按钮
    ///   恢复出一个空 tab；
    /// - 遍历复位 per-tab 状态（含未渲染队列与游标不变量 `frames`/`rendered_up_to`）；
    /// - 对话区提示子 agent 标签页已关闭。
    pub fn cleanup_after_reconnect(&mut self) {
        // 1. 丢弃 subagent tab（复刻关闭逻辑但跳过回收站写入）
        self.tab_bar.discard_subagent_tabs();
        // 2. 遍历复位剩余 tab（主 tab）的本地渲染/流式状态
        for tab in &mut self.tab_bar.tabs {
            tab.reset_after_reconnect();
        }
        // 3. app 级缓存与请求态复位
        self.message_caches.clear();
        self.current_request_id = None;
        self.stale_done_expected = false;
        // 4. 恢复提示（设计 §5.9 文案）
        self.add_message(
            LineType::Status,
            "连接已恢复；子 agent 标签页已关闭（其历史不可恢复）".into(),
        );
    }

    /// 重置为新 session 的状态（保留 textarea 内容）
    pub fn reset_for_new_session(&mut self, session_id: String, model: String, model_key: String) {
        // 重置 active tab 的内容状态（msg/streaming/generating/usage）
        let tab = self.active_tab_mut();
        tab.messages.clear();
        tab.frames.clear();
        tab.streaming_text.clear();
        tab.generating = false;
        tab.pending_usage = None;
        tab.next_message_id = 0;
        tab.scroll = 0;
        tab.rendered_up_to = 0;
        tab.status = AgentStatus::Running;
        // 更新 tab 的 session_id
        tab.session_id = session_id.clone();

        self.message_caches.clear();
        self.stale_done_expected = false;
        self.current_request_id = None;
        self.confirm = None;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.total_cache_creation_input_tokens = 0;
        self.total_cache_read_input_tokens = 0;
        self.total_cost = 0.0;
        self.current_request_usage = (0, 0, 0, 0);
        self.pending_new_session = false;
        self.pending_list_sessions = false;
        self.pending_switch_session = None;
        self.pending_model_select = false;
        self.session_select = None;
        self.model_select = None;
        self.session_id = session_id;
        self.model = model;
        self.model_key = model_key;
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
