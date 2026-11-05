use async_trait::async_trait;
use futures::stream::Stream;
use std::pin::Pin;

use crate::error::LlmError;
use crate::message::{Message, ToolDefinition};

pub use visp_config::{LlmConfig, ModelInfo};

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

/// LLM 流式响应中的事件
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// 文本增量（流式输出的一块）
    TextDelta(String),
    /// 工具调用请求（LLM 要求执行某个工具）
    ToolCall {
        id: String,
        name: String,
        arguments: String, // JSON string
    },
    /// 思考块（如 DeepSeek thinking mode），原样 JSON
    ThinkingBlock(serde_json::Value),
    /// token 用量及工具调用次数
    UsageInfo {
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// 本次请求的累计成本（provider 提供时才有值，如 opencode zen 的 cost 字段）
        cost: Option<f64>,
    },
    /// 逐 chunk 的 token 增量（用于实时速率显示，不参与最终总量结算）
    UsageDelta {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
    /// LLM 响应完成时携带的 ProviderMetadata
    /// 由 provider 在响应全部接收完毕后发射，位于 UsageInfo 之后、Done 之前
    OutputMetadata(crate::ProviderMetadata),
    /// LLM 输出的图片内容块
    ImageBlock {
        /// 图片本地文件路径（base64 来源有值，URL 来源为空字符串）
        path: String,
        /// 图片 MIME 类型（URL 来源为空字符串）
        mime_type: String,
        /// 远程 URL（URL 来源有值，base64 来源为 None）
        remote_url: Option<String>,
    },
    /// 图片处理失败（base64 解码失败、文件写入失败等）
    ImageError {
        /// 失败原因
        reason: String,
    },
    /// 流结束
    Done,
}

/// LLM 提供器抽象 trait
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 流式对话
    /// 发送消息列表，流式接收响应
    ///
    /// `cancel` 为取消令牌：实现方应在阻塞 IO 路径（如 HTTP send）使用
    /// `tokio::select!` 监听该 token，一旦触发立刻返回 `LlmError::Cancelled`，
    /// 让 agent loop 能快速跳出。轻量 mock 可忽略此参数。
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        config: &LlmConfig,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>;
}
