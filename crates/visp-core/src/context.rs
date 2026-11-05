use crate::message::Message;

/// Context 裁剪器 trait
///
/// 定义对话历史裁剪的接口。实现此 trait 的类型负责根据 token 预算
/// 从对话历史中选择合适的子集，并执行工具输出截断。
///
/// 预算计算由实现内部自主完成，调用方只需传入原始参数。
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
