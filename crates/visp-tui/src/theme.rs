//! 全局 UI 主题：颜色、BlockStyle、LineType 前景色映射。
//!
//! 所有 UI 颜色和样式在此统一声明，ui.rs 和 app.rs 从中引用。

use ratatui::style::Color;

use crate::app::LineType;

// ════════════════════════════════════════════════════════════════
// 背景色
// ════════════════════════════════════════════════════════════════

/// 全局聊天背景（深紫黑）
pub const BG: Color = Color::from_u32(0x001A1A2E);

/// 用户消息块底色（深蓝）
pub const USER_BG: Color = Color::from_u32(0x001A3A5E);

/// 助理消息块底色（深灰蓝）
pub const ASSISTANT_BG: Color = Color::from_u32(0x00222A3E);

/// 工具调用/结果块底色（比窗体背景略暗的同色调）
pub const TOOL_BG: Color = Color::from_u32(0x00141428);

/// 子 Agent 调用块底色（深紫色调，与普通工具调用区分）
pub const AGENT_CALL_BG: Color = Color::from_u32(0x002A1A3E);

/// 输入区背景
pub const INPUT_BG: Color = Color::from_u32(0x00111111);

/// 确认栏字体底色
pub const CONFIRM_FONT_BG: Color = Color::from_u32(0x00222222);

/// 阴影色（drop shadow）
pub const SHADOW: Color = Color::from_u32(0x000D0D17);

/// 状态栏背景
pub const STATUS_BG: Color = Color::Black;

// ════════════════════════════════════════════════════════════════
// 前景色
// ════════════════════════════════════════════════════════════════

/// 助理文字
pub const ASSISTANT_FG: Color = Color::White;

/// 用户文字
pub const USER_FG: Color = Color::Cyan;

/// Thinking 文字
pub const THINKING_FG: Color = Color::Green;

/// 工具调用行（首行）
pub const TOOL_CALL_FG: Color = Color::Yellow;

/// 工具结果行 / Usage 行
pub const TOOL_RESULT_FG: Color = Color::DarkGray;

/// 错误文字
pub const ERROR_FG: Color = Color::Red;

/// 状态文字
pub const STATUS_FG: Color = Color::DarkGray;

/// 子 Agent 调用块文字（紫色调，与普通工具调用区分）
pub const AGENT_CALL_FG: Color = Color::Magenta;

/// 独立验证进度/结论（蓝色，与状态行和工具行区分）
pub const VERIFICATION_FG: Color = Color::Cyan;

/// notice 文字
pub const INPUT_NOTICE_FG: Color = Color::DarkGray;
pub const INPUT_FG: Color = Color::White;

/// 确认栏文字
pub const CONFIRM_FG: Color = Color::Yellow;
pub const CONFIRM_BLOCK_BG: Color = Color::DarkGray;
/// 确认栏选中项高亮背景
pub const CONFIRM_SELECTED_BG: Color = Color::from_u32(0x004A6A8E);
/// 确认栏选项标签（[A] 部分）前景色
pub const CONFIRM_OPTION_LABEL_FG: Color = Color::Cyan;
/// 确认栏普通选项文字
pub const CONFIRM_OPTION_FG: Color = Color::White;

// ════════════════════════════════════════════════════════════════
// Tab Bar
// ════════════════════════════════════════════════════════════════

/// Tab 普通文字（与 visp 整体次要文字一致）
pub const TAB_FG: Color = Color::Gray;
/// 选中 tab 的底色（与 USER_BG 同源的深蓝，跟整体协调）
pub const TAB_ACTIVE_BG: Color = USER_BG;
/// 选中 tab 的文字色（Cyan，visp 标志性强调色）
pub const TAB_ACTIVE_FG: Color = Color::Cyan;
/// Tab 之间的分隔符颜色
pub const TAB_DIVIDER_FG: Color = Color::DarkGray;
/// 页码指示器 [N/M] 颜色
pub const TAB_PAGE_FG: Color = Color::DarkGray;

// ════════════════════════════════════════════════════════════════
// 对话区滚动条
// ════════════════════════════════════════════════════════════════

/// 对话区滚动条轨道符号
pub const SCROLLBAR_TRACK: &str = "│";
/// 对话区滚动条滑块符号
pub const SCROLLBAR_THUMB: &str = "▐";
/// 对话区滚动条轨道颜色
pub const SCROLLBAR_TRACK_FG: Color = Color::DarkGray;
/// 对话区滚动条滑块颜色
pub const SCROLLBAR_THUMB_FG: Color = Color::Gray;

// ════════════════════════════════════════════════════════════════
// Help Popup
// ════════════════════════════════════════════════════════════════

/// 帮助弹窗背景（0xCC 半透明，可隐约看到底层内容）
pub const HELP_BG: Color = Color::from_u32(0xCC111122);
/// 帮助弹窗边框
pub const HELP_BORDER_FG: Color = Color::Cyan;
/// 帮助弹窗标题
pub const HELP_TITLE_FG: Color = Color::Cyan;
/// 帮助弹窗小节标题
pub const HELP_SECTION_FG: Color = Color::Yellow;
/// 帮助弹窗快捷键/命令
pub const HELP_KEY_FG: Color = Color::White;
/// 帮助弹窗说明文字
pub const HELP_DESC_FG: Color = Color::DarkGray;
/// 帮助弹窗底部提示
pub const HELP_HINT_FG: Color = Color::DarkGray;

// ════════════════════════════════════════════════════════════════
// Session Select Popup
// ════════════════════════════════════════════════════════════════

/// Session 选择器背景
pub const SELECT_BG: Color = HELP_BG;
/// Session 选择器边框
pub const SELECT_BORDER_FG: Color = HELP_BORDER_FG;
/// Session 选择器标题
pub const SELECT_TITLE_FG: Color = HELP_TITLE_FG;
/// Session 选择器列表项文字
pub const SELECT_ITEM_FG: Color = Color::White;
/// Session 选择器高亮行背景
pub const SELECT_HIGHLIGHT_BG: Color = CONFIRM_SELECTED_BG;

/// 文本选择高亮背景色（浅蓝色）
pub const SELECTION_BG: Color = Color::from_u32(0x004A90D9);

// ════════════════════════════════════════════════════════════════
// BlockStyle — 消息块布局参数
// ════════════════════════════════════════════════════════════════

/// 消息块的统一布局参数。所有消息类型共用同一套渲染流程，差异由此数据驱动。
#[derive(Copy, Clone)]
pub struct BlockStyle {
    /// 顶部空白行（不被 bg_fill 覆盖，纯空气）；用于控制 block 之间的视觉间距
    pub top_margin: u16,
    /// 垂直两端留白（字符数）
    pub margin_vertical: u16,
    /// 水平两端留白（字符数）
    pub margin_horizontal: u16,
    /// 底色；None → bottom_pad 画分隔线，Some → 画底色
    pub bg_fill: Option<Color>,
    /// 是否绘制右侧+底部 drop shadow
    pub shadow: bool,
    /// 内容下方行数（底色或分隔线）
    pub bottom_pad: u16,
}

impl BlockStyle {
    /// 计算该 block 占用的总行数
    pub const fn total_height(self, line_count: u16) -> u16 {
        self.top_margin + self.margin_vertical + line_count + self.bottom_pad
    }
}

pub const USER_STYLE: BlockStyle = BlockStyle {
    top_margin: 2,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: Some(USER_BG),
    shadow: true,
    bottom_pad: 2,
};

pub const ASSISTANT_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: Some(ASSISTANT_BG),
    shadow: true,
    bottom_pad: 2,
};

pub const THINKING_STYLE: BlockStyle = BlockStyle {
    top_margin: 0,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: None,
    shadow: false,
    bottom_pad: 0,
};

/// 工具调用/结果的 fallback 样式（无底色）
pub const TOOL_STYLE: BlockStyle = BlockStyle {
    top_margin: 0,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: None,
    shadow: false,
    bottom_pad: 0,
};

/// 工具调用样式（有背景无阴影）
pub const TOOL_CALL_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: Some(TOOL_BG),
    shadow: false,
    bottom_pad: 1,
};

/// 子 Agent 调用样式（紫色底色，与工具调用同结构）
pub const AGENT_CALL_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 1,
    margin_horizontal: 1,
    bg_fill: Some(AGENT_CALL_BG),
    shadow: false,
    bottom_pad: 1,
};

/// 工具结果样式（纯文本缩进，无底色，从属于调用）
pub const TOOL_RESULT_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 0,
    margin_horizontal: 3,
    bg_fill: None,
    shadow: false,
    bottom_pad: 0,
};

/// 工具错误样式（缩进 2 格，红色底色强调）
pub const TOOL_ERROR_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 0,
    margin_horizontal: 3,
    bg_fill: Some(TOOL_BG),
    shadow: false,
    bottom_pad: 1,
};

/// Usage 统计行（已废弃，保留兼容）
pub const USAGE_STYLE: BlockStyle = BlockStyle {
    top_margin: 1,
    margin_vertical: 0,
    margin_horizontal: 1,
    bg_fill: Some(TOOL_BG),
    shadow: false,
    bottom_pad: 1,
};

// ════════════════════════════════════════════════════════════════
// LineType → 前景色 / BlockStyle 映射
// ════════════════════════════════════════════════════════════════

/// 获取消息类型对应的前景色
pub fn fg_for(line_type: LineType) -> Color {
    match line_type {
        LineType::User => USER_FG,
        LineType::Assistant => ASSISTANT_FG,
        LineType::Thinking => THINKING_FG,
        LineType::ToolCall { .. } => TOOL_CALL_FG,
        LineType::ToolResult { .. } => TOOL_RESULT_FG,
        LineType::ToolError { .. } => ERROR_FG,
        LineType::AgentCall { .. } => AGENT_CALL_FG,
        LineType::Error => ERROR_FG,
        LineType::Status => STATUS_FG,
        LineType::Verification => VERIFICATION_FG,
        LineType::Usage => TOOL_RESULT_FG,
        LineType::Image { .. } => ASSISTANT_FG,
    }
}

/// 获取消息类型对应的 BlockStyle
pub fn style_for(line_type: LineType) -> BlockStyle {
    match line_type {
        LineType::User => USER_STYLE,
        LineType::Assistant => ASSISTANT_STYLE,
        LineType::Thinking => THINKING_STYLE,
        LineType::ToolCall { .. } => TOOL_CALL_STYLE,
        LineType::ToolResult { .. } => TOOL_RESULT_STYLE,
        LineType::ToolError { .. } => TOOL_ERROR_STYLE,
        LineType::AgentCall { .. } => AGENT_CALL_STYLE,
        LineType::Usage => USAGE_STYLE,
        LineType::Error | LineType::Status | LineType::Verification => TOOL_STYLE,
        LineType::Image { .. } => ASSISTANT_STYLE,
    }
}

// (help popup colors removed)
