//! 鼠标文本选择 + OSC 52 剪贴板复制
//!
//! 设计参考 opencode 的 selection.ts + clipboard.ts：
//! - 在 chat area 内拖拽鼠标选择文字，渲染反色高亮
//! - Ctrl+C 将选中文字通过 OSC 52 写入系统粘贴板
//! - Esc 清除选择

use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

/// 文本选择范围（内容坐标：row 包含 scroll 偏移，滚动时高亮跟随文字）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextSelection {
    pub start: Option<(u16, u16)>,
    pub end: Option<(u16, u16)>,
}

impl TextSelection {
    /// 是否有选择范围（用于 Ctrl+C 复制判断）
    pub fn is_active(&self) -> bool {
        self.start.is_some() && self.end.is_some()
    }

    /// 是否有实际选中内容（start != end），用于显示高亮
    pub fn is_highlighting(&self) -> bool {
        self.is_active() && self.start != self.end
    }

    pub fn clear(&mut self) {
        self.start = None;
        self.end = None;
    }

    /// 返回选择矩形区域 (min_col, min_row, max_col, max_row) - 内容坐标
    fn bounds(&self) -> Option<(u16, u16, u16, u16)> {
        let (sc, sr) = self.start?;
        let (ec, er) = self.end?;
        Some((sc.min(ec), sr.min(er), sc.max(ec), sr.max(er)))
    }

    /// 判断某个**屏幕坐标**是否在选择范围内。
    /// `scroll_y` 为当前滚动偏移，用于将屏幕 row 转为内容 row。
    pub fn contains(&self, col: u16, screen_row: u16, scroll_y: u16) -> bool {
        if !self.is_active() {
            return false;
        }
        let (min_c, min_r, max_c, max_r) = match self.bounds() {
            Some(b) => b,
            None => return false,
        };
        // 屏幕坐标 -> 内容坐标
        let row = screen_row + scroll_y;
        if row < min_r || row > max_r {
            return false;
        }
        if min_r == max_r {
            col >= min_c && col <= max_c
        } else if row == min_r {
            col >= min_c
        } else if row == max_r {
            col <= max_c
        } else {
            true
        }
    }
}

/// 将文本写入系统剪贴板
///
/// 优先使用系统原生命令（pbcopy / xclip / wl-copy），
/// 这些在本地终端中比 OSC 52 可靠得多（OSC 52 在 tmux、screen 等
/// 多路复用器中经常被拦截或吞掉）。
pub fn osc52_copy(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // macOS: pbcopy
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }

    // Linux: Wayland (wl-copy) 或 X11 (xclip / xsel)
    #[cfg(target_os = "linux")]
    {
        // wl-copy (Wayland)
        if let Ok(mut child) = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
        // xclip (X11)
        if let Ok(mut child) = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
        // xsel (X11)
        if let Ok(mut child) = Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }

    // Fallback: OSC 52
    let encoded = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x1b\\");
    let mut stdout = std::io::stdout();
    let _ = stdout.flush();
    let _ = write!(stdout, "{seq}");
    let _ = stdout.flush();
}

/// 简易 Base64 编码器
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 2 < data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        result.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        result.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        result.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        result.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = data.len() - i;
    if remaining == 1 {
        let n = (data[i] as u32) << 16;
        result.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        result.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        result.push('=');
        result.push('=');
    } else if remaining == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        result.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        result.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        result.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        result.push('=');
    }
    result
}

/// 从 ratatui Buffer 中提取选择范围内的文本
///
/// 逐行读取，跳过每行末尾的 padding 空格，多行之间用 \n 连接。
/// `scroll_y` 用于将内容坐标转回屏幕坐标。
///
/// `block_rows`（渲染时重建的屏幕行归属表）用于跳过 block 留白行，
/// 并把 block 内容行的提取范围裁剪到内容列内——否则 block 右侧的
/// 边线字符（如 '▕'）会被一并复制出来。
///
/// `chat_area` 为聊天区矩形：最右 1 列是滚动条轨道，任何行都不应
/// 复制到该列；行/列也统一裁剪到聊天区内。
pub fn extract_selected_text(
    buf: &Buffer,
    selection: &TextSelection,
    scroll_y: u16,
    block_rows: &crate::app::BlockRows,
    chat_area: ratatui::layout::Rect,
) -> String {
    if !selection.is_active() {
        return String::new();
    }
    let (min_c, min_r, max_c, max_r) = match selection.bounds() {
        Some(b) => b,
        None => return String::new(),
    };

    // 内容坐标 -> 屏幕坐标
    let min_r_screen = min_r.saturating_sub(scroll_y);
    let max_r_screen = max_r.saturating_sub(scroll_y);

    // 统一裁剪到聊天区：最右 1 列是滚动条轨道，永不复制
    let min_r_screen = min_r_screen.max(chat_area.y);
    let max_r_screen = max_r_screen.min(chat_area.bottom().saturating_sub(1));
    let scroll_col_hi = chat_area.right().saturating_sub(2);

    let area = buf.area();
    let mut lines: Vec<String> = Vec::new();

    for row in min_r_screen..=max_r_screen {
        // block 留白行（边距 / 内边距 / 阴影行）：跳过，不产出文本
        if block_rows.blank.contains(&row) {
            continue;
        }
        // block 内容行：裁剪到内容列范围 (x_start, x_end)（x_end 为排他上界，
        // 与 apply_selection_highlight 的涂色语义一致）
        let (col_lo, col_hi) = match block_rows.content.get(&row).copied() {
            Some((xs, xe)) => (xs, xe.saturating_sub(1)),
            None => (chat_area.x, scroll_col_hi),
        };
        let mut line = String::new();
        let is_first = row == min_r_screen;
        let is_last = row == max_r_screen;
        let col_start = (if is_first { min_c } else { 0 }).max(col_lo);
        let col_end = (if is_last {
            max_c
        } else {
            scroll_col_hi
        })
        .min(col_hi);

        if col_start > col_end {
            continue;
        }

        let mut col = col_start;
        while col <= col_end {
            if col >= area.right() || row >= area.bottom() {
                break;
            }
            let cell = &buf[(col, row)];
            let symbol = cell.symbol();
            let width = UnicodeWidthStr::width(symbol).max(1) as u16;
            line.push_str(symbol);
            col = col.saturating_add(width);
        }
        // 去掉行尾空格（padding）
        let trimmed = line.trim_end();
        lines.push(trimmed.to_string());
    }

    // 移除末尾的空行
    while lines.len() > 1 && lines.last().is_none_or(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
