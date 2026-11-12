use crate::message::Message;
use crate::message::estimate_message_tokens;

/// Context 裁剪器 trait
///
/// 定义对话历史裁剪的接口。实现此 trait 的类型负责根据 token 预算
/// 从对话历史中选择合适的子集，并执行工具输出截断。
///
/// 预算计算由实现内部自主完成，调用方只需传入原始参数。
///
/// # 会话级状态
///
/// 裁剪器实例通过 `Arc` 在多个会话、多个 async task 间共享。需要按会话
/// 保存状态（token 校准系数、溢出压力）的实现应使用 `trim_for_session` /
/// `observe_usage` / `on_context_overflow` 等带 `session_id` 的方法；
/// 默认实现退化为无状态行为，保证旧实现无需改动。
///
/// # Send + Sync
///
/// 裁剪器实例通过 `Arc` 在多个 async task 间共享，
/// 因此要求 `Send + Sync`。
pub trait ContextTrimmer: Send + Sync {
    /// 将对话历史裁剪到上下文窗口内
    ///
    /// # 参数
    ///
    /// - `history`: 待裁剪的对话历史（不含 system message，已过滤 skip_context）
    /// - `max_context_tokens`: LLM 上下文窗口总大小
    /// - `system_overhead`: system prompt + rules + env context 等非历史的 token 开销
    /// - `output_tokens`: 期望的输出 token 数
    ///
    /// # 返回
    ///
    /// 裁剪后的消息列表，总 token 数 ≤ 可用预算。Tool 消息输出已在内部截断。
    fn trim(
        &self,
        history: &[Message],
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
    ) -> Vec<Message>;

    /// 带会话 ID 的裁剪入口。
    ///
    /// 默认忽略 session_id 并调用 `trim`，保持向后兼容。
    fn trim_for_session(
        &self,
        _session_id: Option<&str>,
        history: &[Message],
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
    ) -> Vec<Message> {
        self.trim(history, max_context_tokens, system_overhead, output_tokens)
    }

    /// 用 provider 上报的实际输入 token 校准估算。
    ///
    /// `estimated_input` 必须是未校准的基础估算（通常包含 system prompt、
    /// 历史消息和工具定义），`actual_input` 来自 `ChatEvent::UsageInfo`。
    /// 默认无操作。
    fn observe_usage(&self, _session_id: Option<&str>, _estimated_input: u32, _actual_input: u32) {}

    /// 服务端报告上下文超长后调用。实现可提高安全边际并返回当前溢出级别
    /// （从 0 开始，越大压缩越激进）。默认返回 0 表示不做特殊处理。
    fn on_context_overflow(&self, _session_id: Option<&str>) -> u32 {
        0
    }

    /// LLM 调用成功且拿到 usage 后调用，用于逐步恢复溢出压力。
    /// 默认无操作。
    fn reset_context_pressure(&self, _session_id: Option<&str>) {}

    /// 估算消息列表（含校准系数与每消息开销）。
    /// 默认使用基础 estimator，供日志与测试使用。
    fn estimate_tokens(&self, _session_id: Option<&str>, messages: &[Message]) -> u32 {
        messages.iter().map(estimate_message_tokens).sum()
    }
}

/// 无操作裁剪器：直接返回原始历史，不进行任何裁剪。
/// 可作为默认值使用，或在不需裁剪的场景下使用。
pub struct NoopTrimmer;

impl ContextTrimmer for NoopTrimmer {
    fn trim(
        &self,
        history: &[Message],
        _max_context_tokens: u32,
        _system_overhead: u32,
        _output_tokens: u32,
    ) -> Vec<Message> {
        history.to_vec()
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
