use serde::{Deserialize, Serialize};

/// LLM provider 返回的响应元数据。
///
/// 包含模型名称、结束原因、token 用量及端到端延迟等信息。
/// 序列化为 JSON 后存入 `Message.provider_metadata` 字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderMetadata {
    /// 实际响应的模型名称（如 `"claude-sonnet-4-20250514"`）
    pub model: String,
    /// 结束原因列表（Anthropic 使用 `stop_reason` → `["end_turn"]`；
    /// OpenAI 使用 `finish_reason` → `["stop"]`，统一为 Vec）
    pub finish_reasons: Vec<String>,
    /// 输入 token 数
    pub input_tokens: u32,
    /// 输出 token 数
    pub output_tokens: u32,
    /// 缓存命中的输入 token 数（Anthropic prompt caching）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// 缓存创建的输入 token 数（Anthropic prompt caching）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// 端到端延迟（毫秒），从请求发出到响应完全接收
    pub latency_ms: u64,
}

#[cfg(test)]
#[path = "provider_metadata_tests.rs"]
mod tests;
