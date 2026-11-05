#![allow(dead_code)]

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Tabs, Wrap,
    },
};

use crate::app::{
    AgentStatus, AppState, ConfirmState, LineType, MessageCache, TabEntry, pad_to_width,
};
use crate::debug_log;

/// 将 token 数值格式化（状态栏右侧统计）：
/// 超过 100 → `0.xk`，超过十万 → `0.xm`，超过亿 → `0.xb`（均为一位小数，
/// 取使商大于 0.1 的最大单位）；阈值以内原样显示。
fn format_number(n: u64) -> String {
    if n > 100_000_000 {
        format!("{:.1}b", n as f64 / 1_000_000_000.0)
    } else if n > 100_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n > 100 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

use crate::theme;
use unicode_width::UnicodeWidthStr;

// ════════════════════════════════════════════════════════════════
// Tab Bar 渲染
// ════════════════════════════════════════════════════════════════

/// 为单个 TabEntry 生成标签行：状态符号 + agent_name。
/// `is_active=true` 时用方括号 `[ ... ]` 包裹强调；否则用空格 padding。
fn tab_label_line(tab: &TabEntry, is_active: bool) -> Line<'static> {
    let (symbol, color) = match tab.status {
        AgentStatus::Running => ("▶ ", Color::Yellow),
        AgentStatus::Done => ("✓ ", Color::Green),
        AgentStatus::Error => ("✗ ", Color::Red),
        AgentStatus::ViewOnly => ("◷ ", Color::DarkGray),
    };
    let (lpad, rpad) = if is_active { ("[", "]") } else { (" ", " ") };
    // 子 tab 显示 ✕ 关闭按钮（Running 状态也可关闭，不打断 agent 工作）
    let can_close = !tab.is_main;
    let mut spans = vec![
        Span::raw(lpad),
        Span::styled(symbol, Style::default().fg(color)),
        Span::styled(tab.agent_name.clone(), Style::default().fg(theme::TAB_FG)),
        Span::raw(rpad),
    ];
    if can_close {
        spans.push(Span::styled("✕", Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// 计算单个 tab title 在屏幕上的渲染宽度（cols）。
/// = 前置空格(1) + symbol(2 cols) + agent_name 的 unicode 宽度 + 后置空格(1)
/// + 关闭按钮(1 col, 仅可关闭的子 tab)。
pub(crate) fn tab_label_render_width(tab: &TabEntry) -> u16 {
    // 所有状态的 symbol 都是 "<符号><空格>"，宽度 2
    let symbol_w: u16 = 2;
    let name_w = UnicodeWidthStr::width(tab.agent_name.as_str()) as u16;
    let close_w = if !tab.is_main { 1 } else { 0 };
    1 + symbol_w + name_w + 1 + close_w
}

/// 关闭按钮 ✕ 在 tab label 内的相对列偏移（从 label 起始处算起）。
/// = lpad(1) + symbol(2) + name_w + rpad(1)
/// 返回 None 表示该 tab 没有关闭按钮。
fn tab_close_button_offset(tab: &TabEntry) -> Option<u16> {
    if tab.is_main {
        return None;
    }
    let name_w = UnicodeWidthStr::width(tab.agent_name.as_str()) as u16;
    Some(1 + 2 + name_w + 1) // lpad + symbol + name + rpad
}

/// ratatui Tabs widget 在 title 之间渲染 divider 的宽度（我们用 `│`，1 col）
pub(crate) const TAB_DIVIDER_W: u16 = 1;
/// ratatui Tabs widget 调用 `.padding("", " ")`，左 0，右 1
pub(crate) const TAB_PADDING_L: u16 = 0;
pub(crate) const TAB_PADDING_R: u16 = 1;

/// 在 tabs widget content_area 内，根据点击的相对列号 `rel_x` 命中可见 tab 序号。
///
/// 渲染顺序（来自 ratatui-widgets/Tabs::render_tabs）：对每个 title i，
///   pad_left + title_i + pad_right + (if i<last) divider
/// 命中规则：落在 [pad_left .. pad_left+title_i+pad_right) 区间内的列算命中 i；
/// 落在 divider 上不命中（返回 None）。
///
/// `label_widths` 是按可见顺序传入的每个 title 的渲染宽度（含 tab_label_line 自带的两侧空格）。
pub(crate) fn hit_test_tab_x(rel_x: u16, label_widths: &[u16]) -> Option<usize> {
    if label_widths.is_empty() {
        return None;
    }
    let mut x: u16 = 0;
    let last = label_widths.len() - 1;
    for (i, &w) in label_widths.iter().enumerate() {
        // tab i 的可点击范围 = [x .. x + pad_l + w + pad_r)
        let tab_span = TAB_PADDING_L + w + TAB_PADDING_R;
        if rel_x >= x && rel_x < x + tab_span {
            return Some(i);
        }
        x += tab_span;
        if i < last {
            // divider 区间：不命中
            if rel_x >= x && rel_x < x + TAB_DIVIDER_W {
                return None;
            }
            x += TAB_DIVIDER_W;
        }
    }
    None
}

/// 屏幕坐标 (col,row) → 全局 tab 索引（含 default + 当前页 sub）。
/// 仅当点击落在 tab 内容行（非分隔线行）且命中某个 title（非 divider/页码区）才返回 Some。
pub(crate) fn tab_at_screen(
    tab_bar: &crate::app::TabBar,
    click_col: u16,
    click_row: u16,
) -> Option<usize> {
    // y 必须正好是 tab 内容行
    if click_row != tab_bar.last_tab_area_y {
        return None;
    }
    // x 必须在 content_area 范围内
    if click_col < tab_bar.last_tab_area_x {
        return None;
    }
    let rel_x = click_col - tab_bar.last_tab_area_x;
    if rel_x >= tab_bar.last_term_width {
        return None;
    }

    // 构造当前可见 label 宽度数组：[default, sub_range...]
    let range = tab_bar.current_page_subs(tab_bar.last_term_width);
    let mut widths: Vec<u16> = Vec::with_capacity(1 + range.len());
    widths.push(tab_label_render_width(&tab_bar.tabs[0]));
    for i in range.clone() {
        widths.push(tab_label_render_width(&tab_bar.tabs[i]));
    }

    let visible_idx = hit_test_tab_x(rel_x, &widths)?;
    if visible_idx == 0 {
        Some(0)
    } else {
        Some(range.start + visible_idx - 1)
    }
}

/// 屏幕坐标 (col,row) -> 可关闭的 tab 索引。
/// 仅当点击落在子 tab 的 ✕ 关闭按钮上才返回 Some。
pub(crate) fn tab_close_at_screen(
    tab_bar: &crate::app::TabBar,
    click_col: u16,
    click_row: u16,
) -> Option<usize> {
    if click_row != tab_bar.last_tab_area_y {
        return None;
    }
    if click_col < tab_bar.last_tab_area_x {
        return None;
    }
    let rel_x = click_col - tab_bar.last_tab_area_x;
    if rel_x >= tab_bar.last_term_width {
        return None;
    }

    let range = tab_bar.current_page_subs(tab_bar.last_term_width);

    // 遍历可见 tab，计算每个 tab 的起始 x 和关闭按钮位置
    let mut x: u16 = 0;
    // 主 tab
    let main_w = tab_label_render_width(&tab_bar.tabs[0]);
    x += TAB_PADDING_L + main_w + TAB_PADDING_R + TAB_DIVIDER_W;

    for i in range.clone() {
        let tab = &tab_bar.tabs[i];
        let w = tab_label_render_width(tab);
        let tab_span = TAB_PADDING_L + w + TAB_PADDING_R;

        // 检查关闭按钮
        if let Some(close_off) = tab_close_button_offset(tab) {
            let close_x = x + close_off;
            if rel_x == close_x {
                return Some(i);
            }
        }

        x += tab_span;
        if i < range.end {
            x += TAB_DIVIDER_W;
        }
    }
    None
}

/// 渲染顶部 Tab 栏（2 行：1 行 tab 内容 + 1 行分隔线）
fn render_tab_bar(tab_bar: &mut crate::app::TabBar, f: &mut Frame, area: Rect) {
    // 整条 tab bar 先铺底色（深紫黑），与对话区无缝连贯
    f.render_widget(Block::default().style(Style::default().bg(theme::BG)), area);

    // 2 行布局：tab 内容(1) + 分隔线(1)
    let tab_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab 内容
            Constraint::Length(1), // 分隔线
        ])
        .split(area);

    let content_area = tab_rows[0];
    let sep_area = tab_rows[1];

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(8)])
        .split(content_area);

    // 写入终端宽度供 event.rs 翻页按键使用
    tab_bar.last_term_width = content_area.width;
    tab_bar.last_tab_area_x = content_area.x;
    tab_bar.last_tab_area_y = content_area.y;
    tab_bar.ensure_active_visible(content_area.width);

    let range = tab_bar.current_page_subs(content_area.width);
    let active = tab_bar.active;
    // 所有 visible titles: [default] + 当前页的 sub
    let mut visible: Vec<Line<'static>> = vec![tab_label_line(&tab_bar.tabs[0], active == 0)];
    for i in range.clone() {
        visible.push(tab_label_line(&tab_bar.tabs[i], active == i));
    }

    let tabs = Tabs::new(visible)
        .style(Style::default().bg(theme::BG))
        .select(
            tab_bar
                .select_idx_for_current_page(content_area.width)
                .unwrap_or(0),
        )
        .highlight_style(
            Style::default()
                .bg(theme::TAB_ACTIVE_BG)
                .fg(theme::TAB_ACTIVE_FG)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::styled(
            "│",
            Style::default().fg(theme::TAB_DIVIDER_FG),
        ))
        .padding("", " ");
    f.render_widget(tabs, chunks[0]);

    // 分隔线
    let sep = "─".repeat(sep_area.width as usize);
    f.render_widget(
        Paragraph::new(sep).style(Style::default().fg(SEP_FG)),
        sep_area,
    );

    // 页码指示器 [N/M]，多页时显示
    let pages = tab_bar.layout_pages(content_area.width);
    let page_label = if pages.len() > 1 {
        let current = tab_bar.current_page().min(pages.len().saturating_sub(1)) + 1;
        format!("[{}/{}]", current, pages.len())
    } else {
        String::new()
    };
    let p = Paragraph::new(page_label)
        .style(Style::default().bg(theme::BG).fg(theme::TAB_PAGE_FG))
        .alignment(Alignment::Right);
    f.render_widget(p, chunks[1]);
}

/// 分隔线颜色
const SEP_FG: Color = Color::DarkGray;

/// 计算输入框所需的行数（含文本折行），最小 3 行，最大 10 行
fn calc_input_height(textarea: &ratatui_textarea::TextArea<'static>, width_approx: u16) -> u16 {
    // TextArea 的 block 只有 Borders::TOP（无左右边框），折行宽度 = input_area.width = width_approx
    let content_width = width_approx;
    if content_width < 3 {
        return 3;
    }
    let mut total: u16 = 0;
    for line in textarea.lines() {
        if line.is_empty() {
            total += 1;
        } else {
            let w = UnicodeWidthStr::width(line.as_str()) as u16;
            total += std::cmp::max(1, w.div_ceil(content_width));
        }
    }
    total.clamp(3, 10)
}

/// 计算确认栏所需高度：消息行（含折行）+ 每个选项行（含折行）。
/// 消息前缀 "❓ " 占约 2 列显示宽度。选项前缀 `[A] ` 占 4 列显示宽度。顶部 border 占 1 行。
fn calc_confirm_height(confirm: &ConfirmState, width_approx: u16) -> u16 {
    let msg_prefix_width: usize = 2; // "❓ "
    let opt_prefix_width: usize = 4; // "[A] "

    // 消息行折行计算
    let msg_content_width = if (width_approx as usize) > msg_prefix_width {
        width_approx as usize - msg_prefix_width
    } else {
        1
    };
    let msg_w = UnicodeWidthStr::width(confirm.message.as_str()) as u16;
    let msg_lines = std::cmp::max(1, msg_w.div_ceil(msg_content_width.max(1) as u16));

    // 选项行折行计算
    let opt_content_width = if (width_approx as usize) > opt_prefix_width {
        width_approx as usize - opt_prefix_width
    } else {
        1
    };
    let options: Vec<String> = if confirm.options.is_empty() {
        vec!["Approve".into(), "Deny".into(), "Always Allow".into()]
    } else {
        confirm.options.clone()
    };
    let mut total: u16 = msg_lines; // 消息行（可能多行）
    for opt in &options {
        let w = UnicodeWidthStr::width(opt.as_str()) as u16;
        total += std::cmp::max(1, w.div_ceil(opt_content_width.max(1) as u16));
    }
    if !confirm.options.is_empty() {
        total += 1; // Other 选项行
    }
    total + 1 // 顶部 border
}

/// 顶层渲染入口：将当前 AppState 绘制到终端
pub fn render(app: &mut AppState, f: &mut Frame) {
    // 四周1列留白（上/下不留），绘制背景色
    let area = f.area().inner(ratatui::layout::Margin::new(2, 1));

    let bg = Block::default().style(Style::default().bg(theme::BG));
    f.render_widget(Paragraph::new("").block(bg), f.area());

    // 输入状态与推理状态保持同一几何高度：推理时状态文字显示在输入框内，
    // 不改变输入框的布局高度
    let input_area_height = calc_input_height(&app.textarea, area.width);
    let confirm_height = app
        .confirm
        .as_ref()
        .map(|c| calc_confirm_height(c, area.width))
        .unwrap_or(0);
    let bottom_chunks_height = input_area_height
        + (if app.confirm.is_some() {
            confirm_height + 3
        } else {
            5
        });

    // 纵向分割：Tab栏(2) | 对话区 | 分隔线 | 底部区域
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                    // Tab bar（1 行内容 + 1 行分隔线）
            Constraint::Min(1),                       // 对话区（占满剩余）
            Constraint::Length(1),                    // 分隔线
            Constraint::Length(bottom_chunks_height), // 底部：确认栏/输入区/状态栏
        ])
        .split(area);

    render_tab_bar(&mut app.tab_bar, f, main_chunks[0]);
    render_chat_area(app, f, main_chunks[1]);

    // 分隔线
    let sep_line = "─".repeat(main_chunks[2].width as usize);
    f.render_widget(
        Paragraph::new(sep_line).style(Style::default().fg(SEP_FG)),
        main_chunks[2],
    );

    // 底部区域内部再分割：确认栏(可选) → 输入区 → 状态栏
    let bottom_area = main_chunks[3];
    let bottom_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if app.confirm.is_some() {
            vec![
                Constraint::Length(confirm_height), // 确认栏
                Constraint::Min(2),                 // input area
                Constraint::Length(1),              // 分隔线
                Constraint::Length(2),              // status area
            ]
        } else {
            vec![
                Constraint::Min(2),    // input area
                Constraint::Length(1), // 分隔线
                Constraint::Length(2), // status area
            ]
        })
        .split(bottom_area);

    if app.confirm.is_some() {
        render_confirm_bar(app, f, bottom_chunks[0]);
        render_input_area(app, f, bottom_chunks[1]);
        render_status_bar(app, f, bottom_chunks[3]);
    } else {
        render_input_area(app, f, bottom_chunks[0]);
        render_status_bar(app, f, bottom_chunks[2]);
    }

    // 帮助弹窗（在最上层绘制）
    if app.show_help {
        render_help_popup(f, f.area());
    }

    // Session 选择器弹出面板
    if app.session_select.is_some() {
        render_session_select(f, f.area(), app);
    }

    // 模型选择器弹出面板
    if app.model_select.is_some() {
        render_model_select(f, f.area(), app);
    }

    // 文本选择：拖拽松开后从 buffer 提取选中文字
    // 必须在 apply_selection_highlight 之前执行（此时 buffer 是纯文本，无高亮背景）
    // OSC 52 写入由主循环在 draw() 完成后执行，避免被 ratatui 输出干扰
    if app.pending_copy && app.text_selection.is_active() {
        let text = crate::selection::extract_selected_text(
            f.buffer_mut(),
            &app.text_selection,
            app.scroll_state.y,
            &app.block_rows,
            ratatui::layout::Rect::new(
                app.chat_area_rect.0,
                app.chat_area_rect.1,
                app.chat_area_rect.2,
                app.chat_area_rect.3,
            ),
        );
        if !text.is_empty() {
            app.pending_copy_text = Some(text);
        }
        app.pending_copy = false;
    }

    // 自动清除复制提示（2 秒后）
    if let Some(t) = app.last_copy_time
        && t.elapsed() > std::time::Duration::from_secs(2)
    {
        app.last_copy_msg = None;
        app.last_copy_time = None;
    }

    // 文本选择高亮：对选中区域应用浅蓝色背景
    if app.text_selection.is_highlighting() {
        apply_selection_highlight(f, &app.text_selection, app.scroll_state.y, &app.block_rows);
    }
}

// ════════════════════════════════════════════════════════════════
// 工具函数
// ════════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════════
// 文本选择高亮
// ════════════════════════════════════════════════════════════════

/// 对选中区域的 buffer cells 应用浅蓝色背景
///
/// block 行为约束（ui.md）：拖拽选择时只涂蓝 block 内的内容 ——
/// - 内容行：只涂内容列范围（跳过左右边距列与右侧阴影列）
/// - 留白行（边距/内边距/阴影）：整行跳过
/// - 其余行（block 间空气、输入区等）：保持全宽高亮
fn apply_selection_highlight(
    f: &mut Frame,
    selection: &crate::selection::TextSelection,
    scroll_y: u16,
    block_rows: &crate::app::BlockRows,
) {
    if !selection.is_active() {
        return;
    }
    let buf = f.buffer_mut();
    let area = buf.area();
    let (x, right, y, bottom) = (area.x, area.right(), area.y, area.bottom());
    for row in y..bottom {
        // block 留白行：不涂蓝
        if block_rows.blank.contains(&row) {
            continue;
        }
        // block 内容行：只涂内容列范围
        let span = block_rows.content.get(&row).copied();
        for col in x..right {
            if !selection.contains(col, row, scroll_y) {
                continue;
            }
            if let Some((xs, xe)) = span
                && (col < xs || col >= xe)
            {
                continue;
            }
            let cell = &mut buf[(col, row)];
            cell.set_bg(theme::SELECTION_BG);
        }
    }
}

/// 计算 block 在视窗中的可见范围。不可见返回 None。
///
/// 返回 `(rel_y, visible_h)`：
/// - `rel_y`：相对于视窗顶部的偏移（0 表示视窗第一行）
/// - `visible_h`：在视窗内可见的高度
fn viewport_intersect(
    y: u16,
    h: u16,
    scroll: u16,
    visible: u16,
    _area_bottom: u16,
) -> Option<(u16, u16)> {
    if y + h <= scroll || y >= scroll + visible {
        return None;
    }
    let rel_y = y.saturating_sub(scroll);
    let max_h = h.min(visible.saturating_sub(rel_y));
    if max_h == 0 {
        None
    } else {
        Some((rel_y, max_h))
    }
}

/// 统一渲染一个消息 block。所有消息类型走同一路径。
///
/// 渲染顺序：
///   1. 底部留白/分隔（bottom_pad 行）
///   2. 底色填充（如有）
///   3. 内容 Paragraph
///   4. drop shadow（右侧列 + 底部行）
fn render_block(
    f: &mut Frame,
    area: Rect,
    style: theme::BlockStyle,
    lines: &[Line<'static>],
    line_count: u16,
    rel_y: u16,
    rows: &mut crate::app::BlockRows,
) {
    let content_w = area.width.saturating_sub(1);

    // 顶部 top_margin 行保持空气（不被 bg_fill 覆盖）
    let block_y = rel_y + style.top_margin;
    let mut shadow_y = block_y;
    // 2) 底色填充
    if let Some(bg) = style.bg_fill {
        let buf = f.buffer_mut();
        let end_x = (area.x + content_w).min(buf.area().right());
        let fill_end = (block_y + style.margin_vertical + line_count + style.margin_vertical)
            .min(buf.area().bottom());
        for row in (block_y)..fill_end {
            for x in area.x..end_x {
                buf[(x, row)].set_bg(bg);
            }
        }
        shadow_y = fill_end;
    }

    // 3) 内容 Paragraph
    let content_x = area.x + style.margin_horizontal;
    let content_y = block_y + style.margin_vertical;
    let content_w_adj = content_w.saturating_sub(style.margin_horizontal * 2);
    let actual_lines = line_count.min(area.bottom().saturating_sub(content_y));
    if actual_lines > 0 {
        let p = Paragraph::new(Text::from(lines[..actual_lines as usize].to_vec()));
        f.render_widget(
            p,
            Rect::new(content_x, content_y, content_w_adj, actual_lines),
        );
    }

    // 4) drop shadow（右侧 1 列 + 底部 1 行）
    if style.shadow && actual_lines > 0 && style.bg_fill.is_some() {
        let buf = f.buffer_mut();
        let right = area.right();
        let shadow_col = area.x + content_w; // 阴影起始列
        let bottom_y = shadow_y; // 阴影起始行

        // 右侧阴影：1 列
        for row in content_y..(shadow_y).min(buf.area().bottom()) {
            let x = shadow_col;
            if x < right {
                buf[(x, row)].set_bg(theme::SHADOW);
            }
        }
        // 底部阴影：1 行
        let row = bottom_y;
        if row < buf.area().bottom() {
            for x in (area.x + 1)..=shadow_col {
                if x < right {
                    buf[(x, row)].set_bg(theme::SHADOW);
                }
            }
        }
    }

    // 5) 记录本 block 的屏幕行归属（拖拽选择高亮用）：
    //    内容行 → 内容列范围；留白行（边距/内边距/阴影行）→ 整行跳过高亮。
    //    列范围与第 3 步 Paragraph 的实际渲染区域一致。
    let content_x = area.x + style.margin_horizontal;
    let content_w_adj = content_w.saturating_sub(style.margin_horizontal * 2);
    let content_y = block_y + style.margin_vertical;
    for row in block_y..content_y {
        rows.blank.insert(row);
    }
    for row in content_y..content_y.saturating_add(actual_lines) {
        rows.content
            .insert(row, (content_x, content_x + content_w_adj));
    }
    if style.bg_fill.is_some() {
        let fill_end = (block_y + style.margin_vertical + line_count + style.margin_vertical)
            .min(f.area().bottom());
        for row in content_y.saturating_add(actual_lines)..fill_end {
            rows.blank.insert(row);
        }
        // 底部阴影行（第 4 步实际画出的那一行）
        if style.shadow && actual_lines > 0 && fill_end < f.area().bottom() {
            rows.blank.insert(fill_end);
        }
    }
}

/// 确保所有消息的渲染缓存有效（惰性渲染）。
///
/// 返回是否有缓存被重建或新增（即自上次渲染以来布局可能已变化）。
pub(crate) fn ensure_all_caches(app: &mut AppState, width: u16) -> bool {
    let mut rebuilt = false;
    // Max image height: 40% of visible terminal height, at least 3 rows
    let max_rows = ((app.chat_area_rect.3 as u32 * 40 / 100) as u16).max(3);
    let metrics = crate::image::ImageMetrics {
        font_size: app.image_cache.font_size(),
        image_cache: &app.image_cache,
        max_rows,
    };
    let mut cache_map: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for (i, cache) in app.message_caches.iter().enumerate() {
        cache_map.insert(cache.msg_id, i);
    }
    // Clone to avoid borrow conflict: app.messages() borrows all of &self.
    let msgs = app.messages().to_vec();
    for msg in &msgs {
        let expanded = msg
            .call_id
            .as_ref()
            .map(|cid| app.expanded_tool_calls.contains(cid))
            .unwrap_or(false);
        if let Some(&idx) = cache_map.get(&msg.id) {
            // 图片状态变化（如网络图片下载完成）时也需要重建缓存
            let image_state_changed = match &msg.line_type {
                LineType::Image { path, .. } => {
                    app.message_caches[idx].image_state != app.image_cache.image_state(path)
                }
                _ => false,
            };
            if !app.message_caches[idx].matches(msg, width, expanded) || image_state_changed {
                app.message_caches[idx] =
                    MessageCache::from_message(msg, width, expanded, Some(&metrics));
                rebuilt = true;
            }
        } else {
            app.message_caches.push(MessageCache::from_message(
                msg,
                width,
                expanded,
                Some(&metrics),
            ));
            rebuilt = true;
        }
    }
    app.message_caches
        .retain(|c| msgs.iter().any(|m| m.id == c.msg_id));
    rebuilt
}

// ════════════════════════════════════════════════════════════════
// 对话区渲染
// ════════════════════════════════════════════════════════════════

/// 渲染对话消息列表 + 流式文本。使用统一的 BlockStyle 驱动。
fn render_chat_area(app: &mut AppState, f: &mut Frame, area: Rect) {
    // 保存 chat area 矩形供鼠标命中测试使用
    app.chat_area_rect = (area.x, area.y, area.width, area.height);

    // 重建拖拽选择高亮用的屏幕行归属表（每次渲染全量重建）
    app.block_rows.content.clear();
    app.block_rows.blank.clear();

    let content_w = area.width.saturating_sub(1);

    // ── 调试日志：打印当前 tab 信息及前 5 条消息 ──────────────────
    debug_log!(
        "render_chat_area: active_tab_idx={} session_id={} msg_count={}",
        app.tab_bar.active,
        app.active_tab().session_id,
        app.messages().len(),
    );
    #[cfg(debug_assertions)]
    for (i, msg) in app.messages().iter().take(5).enumerate() {
        let preview = if msg.content.len() > 80 {
            let end = msg.content.floor_char_boundary(80);
            &msg.content[..end]
        } else {
            &msg.content
        };
        debug_log!(
            "  msg[{}]: id={} type={:?} content={}",
            i,
            msg.id,
            &msg.line_type,
            preview.replace('\n', "\\n"),
        );
    }

    // render_block 中 content_w_adj = content_w - margin_horizontal*2
    // 所有 style 的 margin_horizontal 均为 1，按渲染实际宽度折行
    let render_w = content_w.saturating_sub(2);
    ensure_all_caches(app, render_w);
    // 记录当前渲染宽度，供鼠标 hit-test 使用（如 AgentCall 的
    // "[show in new tab]" 按钮命中检测）。若不同步，按钮矩形按宽度 0
    // 计算，点击永远无法命中。
    app.cache_width = render_w;

    const CHAT_PAD: u16 = 0;

    // ── 计算总高度 + 滚动 ────────────────────────────────
    let total: u16 = app
        .messages()
        .iter()
        .map(|m| {
            let style = theme::style_for(m.line_type.clone());
            app.message_caches
                .iter()
                .find(|c| c.msg_id == m.id)
                .map_or(0, |c| style.total_height(c.line_count))
        })
        .sum::<u16>();

    let stream_text = app.active_tab().streaming_display_text();
    let stream_lines = if stream_text.is_empty() {
        0
    } else {
        theme::ASSISTANT_STYLE.total_height(stream_text.lines().count() as u16)
    };

    // task_prompt 的渲染高度（子 tab 第一行）
    let prompt_lines: u16 = app
        .active_tab()
        .task_prompt
        .as_ref()
        .map(|p| {
            let text = format!("📋 {}", p);
            crate::app::wrap_text(&text, render_w).len() as u16
        })
        .unwrap_or(0);
    let prompt_h = if prompt_lines > 0 {
        theme::USER_STYLE.total_height(prompt_lines)
    } else {
        0
    };

    let total_lines = total + stream_lines + prompt_h;
    let visible = area.height;
    let max_scroll = total_lines.saturating_sub(visible);

    if app.scroll_following {
        app.scroll_state.y = max_scroll;
    }
    let scroll_y = app.scroll_state.y.min(max_scroll);
    // 无条件把钳制后的滚动值写回状态：内容变矮（折叠 block、消息合并）时
    // scroll_state.y 可能超出 max_scroll，若不回写，鼠标 hit-test 用的
    // scroll_state.y 与屏幕实际渲染位置不一致，导致点击偏移。
    app.scroll_state.y = scroll_y;

    // ── 统一渲染循环 ──────────────────────────────────────
    let sep_style = Style::default().bg(theme::BG);
    f.render_widget(
        Block::default().style(sep_style),
        Rect::new(area.x, area.y, content_w, CHAT_PAD),
    );

    let mut y: u16 = CHAT_PAD;

    // 子 tab 的 task prompt（渲染为第一条，不依赖 message_caches）
    // clone 出来避免与 &mut app.block_rows 的借用冲突
    let task_prompt = app.active_tab().task_prompt.clone();
    if let Some(ref prompt) = task_prompt {
        let prompt_text = format!("📋 {}", prompt);
        let style = theme::USER_STYLE;
        let wrapped = crate::app::wrap_text(&prompt_text, render_w);
        let lines: Vec<Line<'static>> = wrapped
            .iter()
            .map(|s| {
                Line::styled(
                    crate::app::pad_to_width(s, render_w as usize),
                    Style::default().fg(theme::USER_FG),
                )
            })
            .collect();
        let line_count = lines.len() as u16;
        let h = style.total_height(line_count);
        if let Some((rel_y, visible_h)) = viewport_intersect(y, h, scroll_y, visible, area.bottom())
        {
            let hidden_top = scroll_y.saturating_sub(y);
            let remain = line_count.saturating_sub(hidden_top);
            let visible_lines = remain.min(visible_h);
            if visible_lines > 0 {
                let start = hidden_top as usize;
                let end = (start + visible_lines as usize).min(lines.len());
                render_block(
                    f,
                    area,
                    style,
                    &lines[start..end],
                    visible_lines,
                    area.y + rel_y,
                    &mut app.block_rows,
                );
            }
        }
        y += h;
    }

    for msg in app.messages().to_vec() {
        let style = theme::style_for(msg.line_type.clone());
        if let Some(cache) = app.message_caches.iter().find(|c| c.msg_id == msg.id) {
            let h = style.total_height(cache.line_count);
            if let Some((rel_y, visible_h)) =
                viewport_intersect(y, h, scroll_y, visible, area.bottom())
            {
                let hidden_top = scroll_y.saturating_sub(y);
                let remain = cache.line_count.saturating_sub(hidden_top);
                let visible_lines = remain.min(visible_h);
                if visible_lines > 0 {
                    // Image messages: render via StatefulImage widget
                    if let LineType::Image {
                        ref path,
                        ref alt_text,
                        ref remote_url,
                        ..
                    } = msg.line_type
                    {
                        // Ensure image is loaded: use path as cache key,
                        // or fall back to remote_url when path is empty
                        let cache_key = if path.is_empty() {
                            remote_url.as_deref().unwrap_or("")
                        } else {
                            path.as_str()
                        };
                        if !cache_key.is_empty() {
                            app.image_cache.get_or_load(cache_key);
                        }
                        render_image_block(
                            f,
                            area,
                            &app.image_cache,
                            cache_key,
                            alt_text,
                            remote_url.as_deref(),
                            cache.line_count,
                            visible_lines,
                            area.y + rel_y,
                            render_w,
                            &mut app.block_rows,
                        );
                    } else {
                        let start = hidden_top as usize;
                        let end = (start + visible_lines as usize).min(cache.lines.len());
                        render_block(
                            f,
                            area,
                            style,
                            &cache.lines[start..end],
                            visible_lines,
                            area.y + rel_y,
                            &mut app.block_rows,
                        );
                    }
                }
            }
            y += h;
        }
    }

    // 流式文本
    if !stream_text.is_empty() {
        let lines: Vec<String> = stream_text.lines().map(|s| s.to_string()).collect();
        let line_count = lines.len() as u16;
        let h = theme::ASSISTANT_STYLE.total_height(line_count);
        if let Some((rel_y, visible_h)) = viewport_intersect(y, h, scroll_y, visible, area.bottom())
        {
            let hidden_top = scroll_y.saturating_sub(y);
            let remain = line_count.saturating_sub(hidden_top);
            let visible_lines = remain.min(visible_h);
            if visible_lines > 0 {
                let bs = theme::ASSISTANT_STYLE;
                let content_w_adj = content_w.saturating_sub(bs.margin_horizontal * 2);
                let mut text_lines: Vec<Line> = Vec::new();
                let text_style = Style::default().fg(theme::ASSISTANT_FG);
                for line in &lines[hidden_top as usize..] {
                    if text_lines.len() >= visible_lines as usize {
                        break;
                    }
                    text_lines.push(Line::styled(
                        pad_to_width(line, content_w_adj as usize),
                        text_style,
                    ));
                }
                render_block(f, area, bs, &text_lines, visible_lines, area.y + rel_y, &mut app.block_rows);
            }
        }
    }

    // ── 滚动条（最右 1 列；内容不足一屏时不渲染）─────────────────
    // 该列原本只有 drop shadow（render_block 的 shadow_col），内容
    // Paragraph 不占此列，滚动条最后渲染覆盖其上即可。
    if max_scroll > 0 && area.width > 1 {
        let scrollbar_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        // 记录滑块几何供滚动条拖拽命中测试（与下方组件共用同一计算，
        // 保证渲染位置与拖拽映射一致）。content_length 语义为"可滚动
        // 位置数"（max_scroll + 1），滚到底时滑块贴住轨道底部。
        let (thumb_start, thumb_len) =
            crate::app::scrollbar_thumb_geometry(total_lines, visible, scroll_y, area.height);
        app.scrollbar_geo = Some(crate::app::ScrollbarGeo {
            area: scrollbar_area,
            thumb_start,
            thumb_len,
            max_scroll,
        });
        let mut scrollbar_state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll_y as usize)
            .viewport_content_length(visible as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some(theme::SCROLLBAR_TRACK))
            .thumb_symbol(theme::SCROLLBAR_THUMB)
            .track_style(Style::default().fg(theme::SCROLLBAR_TRACK_FG))
            .thumb_style(Style::default().fg(theme::SCROLLBAR_THUMB_FG));
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    } else {
        app.scrollbar_geo = None;
    }
}

// ════════════════════════════════════════════════════════════════
// 图片块渲染
// ════════════════════════════════════════════════════════════════

/// Render an image block in the chat area.
///
/// - Ready: renders via stateless `Image` widget with fixed size (no mouse zoom)
/// - Loading: renders `[加载中: path]` placeholder text
/// - Error: renders `[图片加载失败: path (reason)]` error text
///
/// Address lines (🔗 url / 📁 path) are rendered below the image when available.
#[allow(clippy::too_many_arguments)]
fn render_image_block(
    f: &mut Frame,
    area: Rect,
    image_cache: &crate::image::ImageCache,
    path: &str,
    alt_text: &str,
    remote_url: Option<&str>,
    line_count: u16,
    visible_lines: u16,
    y: u16,
    render_w: u16,
    rows: &mut crate::app::BlockRows,
) {
    let img_h = visible_lines.min(line_count);

    // Address lines to show below the image (or placeholder)
    let mut addr_lines: Vec<Line<'static>> = Vec::new();
    if let Some(url) = remote_url.filter(|u| !u.is_empty()) {
        let display = format!("🔗 {}", url);
        for wrapped in crate::app::wrap_text(&display, render_w) {
            addr_lines.push(Line::styled(wrapped, Style::default().fg(Color::DarkGray)));
        }
    }
    if !path.is_empty() {
        let display = format!("📁 {}", path);
        for wrapped in crate::app::wrap_text(&display, render_w) {
            addr_lines.push(Line::styled(wrapped, Style::default().fg(Color::DarkGray)));
        }
    }
    let addr_count = addr_lines.len() as u16;
    let image_h = img_h.saturating_sub(addr_count);

    // Render image only if there's positive height left after address lines
    if image_h > 0 {
        if let Some(protocol_arc) = image_cache.try_get_protocol(path, (render_w, image_h)) {
            // Ready: render stateless Image widget (fixed size, no mouse interaction)
            let image_area = Rect::new(
                area.x + 1, // margin_horizontal = 1
                y,
                render_w,
                image_h,
            );
            if image_area.width > 0
                && image_area.height > 0
                && let Ok(protocol) = protocol_arc.lock()
            {
                use ratatui_image::Image;
                f.render_widget(Image::new(&protocol), image_area);
            }
        } else if image_cache.is_loading(path) {
            // Loading: show placeholder
            let msg = format!("[加载中: {}]", alt_text);
            let line = Line::styled(msg, Style::default().fg(Color::DarkGray));
            let bs = theme::ASSISTANT_STYLE;
            render_block(f, area, bs, &[line], image_h, y, rows);
        } else {
            // Error: show error message
            let reason = image_cache.error_message(path).unwrap_or_default();
            let msg = format!("[图片加载失败: {} ({})]", alt_text, reason);
            let line = Line::styled(msg, Style::default().fg(Color::Red));
            let bs = theme::ASSISTANT_STYLE;
            render_block(f, area, bs, &[line], image_h, y, rows);
        }
    }

    // Render address lines below the image
    if !addr_lines.is_empty() {
        for (i, line) in addr_lines.iter().enumerate() {
            let addr_y = y + image_h + i as u16;
            if addr_y < area.bottom() {
                f.render_widget(line.clone(), Rect::new(area.x + 1, addr_y, render_w, 1));
            }
        }
    }

    // 记录图片 block 的内容行（拖拽选择高亮用）：图片 + 地址行均为内容，
    // 列范围与实际渲染区域一致（margin_horizontal = 1）
    let content_x = area.x + 1;
    let content_end = (content_x + render_w).min(f.area().right());
    for row in y..y.saturating_add(image_h).min(area.bottom()) {
        rows.content.insert(row, (content_x, content_end));
    }
    for i in 0..addr_count {
        let addr_y = y + image_h + i;
        if addr_y < area.bottom() {
            rows.content.insert(addr_y, (content_x, content_end));
        }
    }
}

// ════════════════════════════════════════════════════════════════
// 底部区域组件
// ════════════════════════════════════════════════════════════════

/// 确认栏：工具调用需用户确认时显示
fn render_confirm_bar(app: &AppState, f: &mut Frame, area: Rect) {
    if let Some(ref confirm) = app.confirm {
        // 构建选项标签
        let options: Vec<String> = if confirm.options.is_empty() {
            vec!["Approve".into(), "Deny".into(), "Always Allow".into()]
        } else {
            confirm.options.clone()
        };
        let has_other = !confirm.options.is_empty();
        let num_opts = options.len();

        // 每个选项的字母标签 [A] [B] ...
        let letters: Vec<char> = (0..num_opts + if has_other { 1 } else { 0 })
            .map(|i| (b'A' + i as u8) as char)
            .collect();

        // 消息行
        let msg_line = Line::from(vec![
            Span::styled("❓ ", Style::default().fg(theme::CONFIRM_FG)),
            Span::styled(&confirm.message, Style::default().fg(theme::ASSISTANT_FG)),
        ]);

        // 构建竖排选项行：每个选项占一行，支持折行
        let mut lines: Vec<Line> = vec![msg_line];

        for (i, opt) in options.iter().enumerate() {
            let is_selected = i == confirm.selected_index && !confirm.other_active;
            let letter = letters[i];
            let prefix = format!("[{}] ", letter);
            let opt_line = build_option_line(&prefix, opt, is_selected);
            lines.push(opt_line);
        }

        if has_other {
            let other_idx = num_opts;
            let is_selected = other_idx == confirm.selected_index && !confirm.other_active;
            let letter = letters[other_idx];
            let prefix = format!("[{}] ", letter);
            let opt_line = build_option_line(&prefix, "Other", is_selected);
            lines.push(opt_line);
        }

        let text = Text::from(lines);

        let p = Paragraph::new(text)
            .style(Style::default().bg(theme::CONFIRM_FONT_BG))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme::CONFIRM_BLOCK_BG)),
            );
        f.render_widget(p, area);
    }
}

/// 构建单个选项行（竖排模式）：前缀 `[A] ` 和选项文本分别着色，选中时整行高亮。
fn build_option_line(prefix: &str, text: &str, is_selected: bool) -> Line<'static> {
    if is_selected {
        Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default()
                    .fg(theme::CONFIRM_FG)
                    .bg(theme::CONFIRM_SELECTED_BG),
            ),
            Span::styled(
                text.to_string(),
                Style::default()
                    .fg(theme::CONFIRM_FG)
                    .bg(theme::CONFIRM_SELECTED_BG),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(theme::CONFIRM_OPTION_LABEL_FG),
            ),
            Span::styled(
                text.to_string(),
                Style::default().fg(theme::CONFIRM_OPTION_FG),
            ),
        ])
    }
}

/// 输入区：tui-textarea 封装
fn render_input_area(app: &mut AppState, f: &mut Frame, area: Rect) {
    let input_area = Rect::new(area.x, area.y, area.width, area.height);

    // debug: log textarea state before rendering
    let _line_count = app.textarea.lines().len();
    let _total_chars: usize = app.textarea.lines().iter().map(|l| l.len()).sum();
    debug_log!(
        "render_input_area: input_area.width={}, lines={}, total_chars={}",
        input_area.width,
        _line_count,
        _total_chars
    );

    // 非 default tab 时输入框视觉禁用（行为禁用在 event.rs TODO Step 11）
    let input_disabled = app.tab_bar.active != 0;

    let is_other_mode = app.confirm.as_ref().is_some_and(|c| c.other_active);

    if input_disabled {
        if app.active_tab().status == AgentStatus::ViewOnly {
            // ViewOnly tab：显示只读提示
            app.textarea.set_style(Style::default().fg(Color::DarkGray));
            app.textarea
                .set_placeholder_text("此 tab 为只读历史，无法输入");
        } else {
            // 其他 sub tab：显示灰色提示
            app.textarea.set_style(Style::default().fg(Color::DarkGray));
            app.textarea.set_placeholder_text("切回 default tab 输入");
        }
    } else if is_other_mode {
        app.textarea.set_style(Style::default().fg(theme::INPUT_FG));
        app.textarea
            .set_placeholder_text("Type your custom input...");
    } else if app.generating() && app.confirm.is_none() {
        // 推理中：输入框几何高度不变，状态文字以 placeholder 显示在框内
        let tps = match app.active_tab().tokens_per_second() {
            Some(v) => format!("{v:.1}"),
            None => "--".to_string(),
        };
        app.textarea
            .set_style(Style::default().fg(theme::INPUT_NOTICE_FG));
        app.textarea.set_placeholder_text(format!(
            "Generating {}  {tps} tokens/s output",
            app.spinner_glyph()
        ));
    } else {
        app.textarea.set_style(Style::default().fg(theme::INPUT_FG));
        app.textarea.set_placeholder_text("Type your message...");
        // 命令提示
        let current = app
            .textarea
            .lines()
            .first()
            .map(|s| s.as_str())
            .unwrap_or("");
        if current.starts_with('/') {
            let all_cmds = [
                "/clear",
                "/help",
                "/init",
                "/init-agent",
                "/init-skill",
                "/list",
                "/model",
                "/new",
                "/reload",
                "/sessions",
                "/temp",
            ];
            let hint: Vec<&str> = if current.len() > 1 {
                all_cmds
                    .iter()
                    .filter(|c| c.starts_with(current))
                    .copied()
                    .collect()
            } else {
                all_cmds.to_vec()
            };
            if !hint.is_empty() {
                let hint_line = format!("  {}", hint.join("  "));
                let hint_y = area.y + area.height.saturating_sub(1);
                if hint_y > area.y {
                    f.render_widget(
                        Paragraph::new(hint_line)
                            .style(Style::default().fg(theme::INPUT_NOTICE_FG)),
                        Rect::new(area.x, hint_y, area.width, 1),
                    );
                }
            }
        }
    }
    app.textarea
        .set_block(Block::default().style(Style::default().bg(theme::INPUT_BG)));
    // 单次渲染：既设置内部 area（折行/导航所需的 screen map），也完成可视输出
    f.render_widget(&app.textarea, input_area);
}

/// 将 model 字符串拆为 (provider, model_label)
/// 期望格式为 "{provider}/{name}"，如 "Ollama/deepseek-v4-flash"
/// 无分隔斜杠时 provider 为空字符串
fn split_model_name(model: &str) -> (&str, &str) {
    model.split_once('/').unwrap_or(("", model))
}

/// 格式化状态栏左侧字符串
fn format_status_left(session_id: &str, model_key: &str, generating: bool) -> String {
    let sid: String = session_id.chars().take(8).collect();
    let status = if generating { "Generating" } else { "Idle" };
    let (provider, model_label) = split_model_name(model_key);
    format!(
        "{sid} | {model}({provider}) | {status}",
        sid = sid,
        model = model_label,
        provider = provider
    )
}

/// 格式化状态栏第 2 行右侧 token / cache 统计
fn format_status_tokens(app: &AppState) -> String {
    let base = format!(
        "Tokens: {} i / {} o | Cache: {} c / {} r",
        format_number(app.total_input_tokens as u64),
        format_number(app.total_output_tokens as u64),
        format_number(app.total_cache_creation_input_tokens as u64),
        format_number(app.total_cache_read_input_tokens as u64),
    );
    // 成本仅在 provider 上报过（> 0）时展示，避免未知成本时显示误导性的 $0
    if app.total_cost > 0.0 {
        format!("{base} | Cost: ${:.4}", app.total_cost)
    } else {
        base
    }
}

/// 按显示宽度压缩 workdir 路径（状态栏第 2 行左侧）：
/// 优先全长度；超长时先压缩中间路径段（保留头尾、中间以 "..." 替代）；
/// 仍超长则压缩头部，仅保留尾部，形如 ".../xxx"。
fn compress_workdir(path: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_string();
    }
    // 阶段 1：压缩中间路径段。从压缩 1 段开始逐步扩大范围，
    // 保留尽可能多的头部与尾部路径段。
    let (prefix, core) = match path.strip_prefix('/') {
        Some(rest) => ("/", rest),
        None => ("", path),
    };
    let segs: Vec<&str> = core.split('/').collect();
    let n = segs.len();
    if n >= 2 {
        // keep = 保留的路径段总数（含首尾），从 n-1 递减到 2
        for keep in (2..n).rev() {
            let head_keep = keep / 2;
            let tail_keep = keep - head_keep;
            let candidate = format!(
                "{}{}/.../{}",
                prefix,
                segs[..head_keep].join("/"),
                segs[n - tail_keep..].join("/")
            );
            if UnicodeWidthStr::width(candidate.as_str()) <= max_width {
                return candidate;
            }
        }
    }
    compress_path_head(path, max_width)
}

/// 压缩路径头部，仅保留尾部（".../xxx" 形式）：
/// 优先以 '/' 为起点保留完整目录后缀；无可用 '/' 边界时退化为尾部字符截断。
fn compress_path_head(path: &str, max_width: usize) -> String {
    let dots = UnicodeWidthStr::width("...");
    let budget = max_width.saturating_sub(dots);
    if budget == 0 {
        return "...".to_string();
    }
    // 优先保留以 '/' 开头的最长可行后缀
    for (i, c) in path.char_indices() {
        if c == '/' && UnicodeWidthStr::width(&path[i..]) <= budget {
            return format!("...{}", &path[i..]);
        }
    }
    // 无 '/' 边界：取宽度允许的最长字符后缀
    let chars: Vec<char> = path.chars().collect();
    for s in 0..chars.len() {
        let suffix: String = chars[s..].iter().collect();
        if UnicodeWidthStr::width(suffix.as_str()) <= budget {
            return format!("...{}", suffix);
        }
    }
    "...".to_string()
}

/// 渲染状态栏单行：左侧左对齐（STATUS 样式），右侧右对齐（TOOL_RESULT 样式）
fn render_status_row(f: &mut Frame, area: Rect, left_text: &str, right_text: &str) {
    let right_w = UnicodeWidthStr::width(right_text) as u16;
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(right_w)])
        .split(area);

    let left = Paragraph::new(left_text)
        .style(Style::default().fg(theme::STATUS_FG).bg(theme::STATUS_BG))
        .block(Block::default());
    f.render_widget(left, chunks[0]);

    let right = Paragraph::new(right_text)
        .style(
            Style::default()
                .fg(theme::TOOL_RESULT_FG)
                .bg(theme::STATUS_BG),
        )
        .alignment(Alignment::Right)
        .block(Block::default());
    f.render_widget(right, chunks[1]);
}

/// 底部状态栏（2 行）：第 1 行会话/模型/状态 + 快捷键，第 2 行工作目录 + token 统计
fn render_status_bar(app: &mut AppState, f: &mut Frame, area: Rect) {
    // 记录第 1 行屏幕起点，供鼠标点击复制 session_id 命中判定使用
    app.status_bar_first_row = Some((area.x, area.y));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    // 复制提示激活时，状态栏第 1 行左侧显示复制信息
    let left1 = if app.last_copy_msg.is_some() {
        app.last_copy_msg.clone().unwrap_or_default()
    } else {
        format_status_left(&app.session_id, &app.model_key, app.generating())
    };
    render_status_row(f, rows[0], &left1, "Ctrl-d: quit | /help = help");

    // 第 2 行：workdir 动态压缩 —— 优先全长度；超长（workdir + 4 空格间隔 +
    // Tokens 数据 > 一行宽度）时先压缩中间、仍超长再压缩头部（".../xxx"）
    let tokens = format_status_tokens(app);
    let tokens_w = UnicodeWidthStr::width(tokens.as_str()) as u16;
    let workdir_max = area.width.saturating_sub(tokens_w + 4) as usize;
    let workdir = compress_workdir(&app.project_path, workdir_max);
    render_status_row(f, rows[1], &workdir, &tokens);
}

// ════════════════════════════════════════════════════════════════
// 帮助弹窗
// ════════════════════════════════════════════════════════════════

/// 渲染帮助弹窗覆盖层，居中显示，内容为所有命令和快捷键
fn render_help_popup(f: &mut Frame, area: Rect) {
    let cmd_items = [
        ("/clear", "Clear chat history"),
        ("/help", "Show this help popup"),
        ("/list", "List all sessions"),
        ("/sessions <id>", "Switch to a session by short-id"),
        ("/new", "Start a new session"),
        ("/reload", "Reload filesystem assets (rules/skills/agents)"),
        ("/temp <n>", "Set temperature (0.0–1.0)"),
        ("/model <m>", "Switch model"),
        ("/init", "Initialize session with system prompt"),
        (
            "/init-agent <name>",
            "Create a template agent file in .visp/agents/",
        ),
        (
            "/init-skill <name>",
            "Create a template skill file in .visp/skills/",
        ),
    ];
    let key_items = [
        ("F1 / /help", "Toggle this help popup"),
        ("Enter", "Send message / confirm"),
        ("Alt+Enter", "Insert newline (without sending)"),
        ("↑ / ↓", "Input history navigation"),
        ("PgUp / PgDn", "Scroll chat 10 lines"),
        ("Tab", "Command auto-completion"),
        ("Alt+, / Alt+.", "Cycle Sub-Agent tabs"),
        ("Alt+Shift+← / →", "Tab pagination"),
        ("Ctrl+W", "Close finished Sub-Agent tab"),
        ("Ctrl+C", "Cancel generation / clear input"),
        ("Ctrl+D", "Quit"),
        ("← / →", "Switch option in confirm dialog"),
        ("Esc", "Cancel / clear text selection"),
        ("Mouse drag", "Select & auto-copy to clipboard"),
        ("Mouse wheel", "Scroll chat area"),
    ];

    let mut lines: Vec<Line<'static>> = Vec::new();

    // 命令小节
    lines.push(Line::from(Span::styled(
        " Commands:",
        Style::default().fg(theme::HELP_SECTION_FG),
    )));
    lines.push(Line::from(""));
    for (key, desc) in &cmd_items {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", key),
                Style::default().fg(theme::HELP_KEY_FG),
            ),
            Span::styled(*desc, Style::default().fg(theme::HELP_DESC_FG)),
        ]));
    }

    // 热重载边界说明：AGENTS.md/rules、skills、agents 保存自动生效；
    // daemon.toml 变更需重启 daemon（设计 §5.8）
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Note: AGENTS.md/rules, skills, agents",
        Style::default().fg(theme::HELP_DESC_FG),
    )));
    lines.push(Line::from(Span::styled(
        " auto-reload on save; daemon.toml changes",
        Style::default().fg(theme::HELP_DESC_FG),
    )));
    lines.push(Line::from(Span::styled(
        " require a daemon restart",
        Style::default().fg(theme::HELP_DESC_FG),
    )));

    lines.push(Line::from(""));
    lines.push(Line::from(""));

    // 快捷键小节
    lines.push(Line::from(Span::styled(
        " Shortcuts:",
        Style::default().fg(theme::HELP_SECTION_FG),
    )));
    lines.push(Line::from(""));
    for (key, desc) in &key_items {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", key),
                Style::default().fg(theme::HELP_KEY_FG),
            ),
            Span::styled(*desc, Style::default().fg(theme::HELP_DESC_FG)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Press any key or click to close",
        Style::default().fg(theme::HELP_HINT_FG),
    )));

    // 计算弹窗尺寸
    let popup_width = 46.min(area.width.saturating_sub(4));
    let popup_height = (lines.len() + 2) as u16; // +2 for top/bottom border
    let popup_height = popup_height.min(area.height.saturating_sub(4));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;

    let popup_area = Rect::new(x, y, popup_width, popup_height);

    // 背景覆盖层
    let overlay = Block::default().style(Style::default().bg(theme::HELP_BG));
    f.render_widget(overlay, popup_area);

    // 弹窗主体（带边框）
    let popup = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::HELP_BORDER_FG))
                .title(" Help ")
                .title_style(Style::default().fg(theme::HELP_TITLE_FG))
                .style(Style::default().bg(theme::HELP_BG)),
        )
        .style(Style::default().bg(theme::HELP_BG));
    f.render_widget(popup, popup_area);
}

// ════════════════════════════════════════════════════════════════
// Session 选择器弹出面板
// ════════════════════════════════════════════════════════════════

/// 渲染 Session 选择器弹出面板（/list 或 /sessions 无参触发）
fn render_session_select(f: &mut Frame, area: Rect, app: &mut AppState) {
    use crate::theme;
    use ratatui::style::Modifier;

    if let Some(ref mut ss) = app.session_select {
        let session_count = ss.session_ids.len();
        let content_height = (session_count as u16).clamp(5, 20);
        let popup_width = (area.width * 3 / 4).clamp(50, 100);
        let popup_height = content_height + 4;

        let x = (area.width.saturating_sub(popup_width)) / 2;
        let y = (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_height);

        // 清除背景
        f.render_widget(Clear, popup_area);

        // 构建 ListItems
        let items: Vec<ListItem> = ss
            .labels
            .iter()
            .map(|label| {
                ListItem::new(Span::styled(
                    label.as_str(),
                    Style::default().fg(theme::SELECT_ITEM_FG),
                ))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(Span::styled(
                        " Sessions ",
                        Style::default().fg(theme::SELECT_TITLE_FG),
                    )))
                    .title_bottom(Line::from(Span::styled(
                        " ↑↓ navigate  Enter switch  Esc/q cancel ",
                        Style::default().fg(theme::HELP_HINT_FG),
                    )))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::SELECT_BORDER_FG))
                    .style(Style::default().bg(theme::SELECT_BG)),
            )
            .highlight_style(
                Style::default()
                    .bg(theme::SELECT_HIGHLIGHT_BG)
                    .add_modifier(Modifier::BOLD),
            );

        f.render_stateful_widget(list, popup_area, &mut ss.state);
    }
}

/// 渲染模型选择器弹出面板（/model 无参触发）
fn render_model_select(f: &mut Frame, area: Rect, app: &mut AppState) {
    use crate::theme;
    use ratatui::style::Modifier;

    if let Some(ref mut ms) = app.model_select {
        let model_count = ms.display_labels.len();
        let content_height = (model_count as u16).clamp(3, 15);
        let popup_width = (area.width / 2).clamp(36, 80);
        let popup_height = content_height + 4;

        let x = (area.width.saturating_sub(popup_width)) / 2;
        let y = (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_height);

        f.render_widget(Clear, popup_area);

        let items: Vec<ListItem> = ms
            .display_labels
            .iter()
            .map(|label| {
                ListItem::new(Span::styled(
                    format!("  {label}"),
                    Style::default().fg(theme::SELECT_ITEM_FG),
                ))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(Span::styled(
                        " Model ",
                        Style::default().fg(theme::SELECT_TITLE_FG),
                    )))
                    .title_bottom(Line::from(Span::styled(
                        " ↑↓ navigate  Enter switch  Esc/q cancel ",
                        Style::default().fg(theme::HELP_HINT_FG),
                    )))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::SELECT_BORDER_FG))
                    .style(Style::default().bg(theme::SELECT_BG)),
            )
            .highlight_style(
                Style::default()
                    .bg(theme::SELECT_HIGHLIGHT_BG)
                    .add_modifier(Modifier::BOLD),
            );

        f.render_stateful_widget(list, popup_area, &mut ms.state);
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
