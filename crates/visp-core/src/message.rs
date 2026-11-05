use crate::ProviderMetadata;
use serde::{Deserialize, Serialize};

/// 消息子类型，与 role 正交
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MessageType {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "thinking")]
    Thinking,
    #[serde(rename = "tool_call")]
    ToolCall,
    #[serde(rename = "tool_result")]
    ToolResult,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
}

/// 对话角色
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Role {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
}

/// 工具调用（来自 LLM）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub arguments: String, // JSON string
}

/// 用户附带的图片数据（用于多模态 vision 请求）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageData {
    /// 图片本地文件路径
    pub path: String,
    /// base64 编码的图片数据（不含 data: 前缀）
    pub base64: String,
    /// MIME 类型（如 "image/png"）
    pub mime_type: String,
}

/// 对话中的一条消息
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    /// 消息子类型（Text/Thinking/ToolCall/ToolResult/Error/Status/System/User）
    pub kind: MessageType,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    /// 额外的原始内容块（如 thinking），以 JSON Value 存储
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_blocks: Option<Vec<serde_json::Value>>,
    /// 标记此消息不注入后续对话的 context window（用于 /init 等系统命令）
    #[serde(default)]
    pub skip_context: bool,
    /// 估算的消息 token 数，用于 context window 管理
    #[serde(default)]
    pub estimated_tokens: u32,
    /// 实际输入 token（来自 LLM provider）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_tokens_input: Option<u32>,
    /// 实际输出 token（来自 LLM provider）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_tokens_output: Option<u32>,
    /// 实际 cache read token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_cache_read: Option<u32>,
    /// 实际 cache write token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_cache_write: Option<u32>,
    /// 实际费用（美元）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_cost: Option<f64>,
    /// LLM 提供商元数据 JSON
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
    /// 工具执行是否出错（仅 tool 消息）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_is_error: Option<bool>,
    /// 工具执行耗时（毫秒，仅 tool 消息）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_duration_ms: Option<u64>,
    /// 工具调用次数（仅 assistant 的 ToolCall 消息）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<u32>,
    /// 创建时间（Unix 毫秒，用于 DB 持久化）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// 用户附带的图片（多模态 vision 请求）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageData>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        let mut msg = Self {
            role: Role::System,
            kind: MessageType::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        };
        msg.estimated_tokens = estimate_message_tokens(&msg);
        msg
    }

    pub fn user(content: impl Into<String>) -> Self {
        let mut msg = Self {
            role: Role::User,
            kind: MessageType::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        };
        msg.estimated_tokens = estimate_message_tokens(&msg);
        msg
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::assistant_with_metadata(content, None)
    }

    /// 构造 assistant 消息，同时附带 provider 元数据
    ///
    /// `provider_metadata` 会被序列化为 JSON 存入 `Message.provider_metadata` 字段。
    pub fn assistant_with_metadata(
        content: impl Into<String>,
        provider_metadata: Option<ProviderMetadata>,
    ) -> Self {
        let mut msg = Self {
            role: Role::Assistant,
            kind: MessageType::Text,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: provider_metadata
                .map(|pm| serde_json::to_value(pm).expect("ProviderMetadata serialization")),
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        };
        msg.estimated_tokens = estimate_message_tokens(&msg);
        msg
    }

    pub fn tool(content: impl Into<String>, call_id: impl Into<String>) -> Self {
        Self::tool_with_duration(content, call_id, None)
    }

    /// 构造 tool 结果消息，并附带工具执行耗时（毫秒）
    pub fn tool_with_duration(
        content: impl Into<String>,
        call_id: impl Into<String>,
        duration_ms: Option<u64>,
    ) -> Self {
        let mut msg = Self {
            role: Role::Tool,
            kind: MessageType::ToolResult,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: duration_ms,
            created_at: None,
            images: Vec::new(),
        };
        msg.estimated_tokens = estimate_message_tokens(&msg);
        msg
    }

    pub fn tool_call(calls: Vec<ToolCallRequest>) -> Self {
        Self {
            role: Role::Assistant,
            kind: MessageType::ToolCall,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(calls),
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        }
    }

    pub fn thinking(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            kind: MessageType::Thinking,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            kind: MessageType::Error,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        }
    }

    pub fn status(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            kind: MessageType::Status,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
            tool_call_count: None,
            extra_blocks: None,
            skip_context: false,
            estimated_tokens: 0,
            actual_tokens_input: None,
            actual_tokens_output: None,
            actual_cache_read: None,
            actual_cache_write: None,
            actual_cost: None,
            provider_metadata: None,
            tool_result_is_error: None,
            tool_result_duration_ms: None,
            created_at: None,
            images: Vec::new(),
        }
    }

    /// 从文本中解析 `<image: path>` 标记，读取图片文件并 base64 编码。
    /// 返回 (清理后的文本, 图片数据列表)。
    /// URL 标记（`<image: | url>`）不处理，保留在文本中。
    pub fn extract_images(text: &str) -> (String, Vec<ImageData>) {
        let mut images = Vec::new();
        let mut clean_text = String::new();
        let mut search_from = 0;

        while let Some(rel_start) = text[search_from..].find("<image: ") {
            let marker_start = search_from + rel_start;
            let path_start = marker_start + "<image: ".len();

            if let Some(rel_end) = text[path_start..].find('>') {
                let marker_end = path_start + rel_end;
                let raw = text[path_start..marker_end].trim();

                // Only process local path markers (no `|` separator)
                if !raw.contains('|') && !raw.is_empty() {
                    // Append text before marker
                    clean_text.push_str(&text[search_from..marker_start]);

                    // Read and encode image file
                    if let Ok(file_bytes) = std::fs::read(raw) {
                        let mime_type = guess_mime_type(raw);
                        let base64 = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &file_bytes,
                        );
                        images.push(ImageData {
                            path: raw.to_string(),
                            base64,
                            mime_type,
                        });
                    }
                    // If file read fails, silently skip the marker
                } else {
                    // URL marker or empty - keep original text
                    clean_text.push_str(&text[search_from..marker_end + 1]);
                }

                search_from = marker_end + 1;
            } else {
                break;
            }
        }

        // Append remaining text
        clean_text.push_str(&text[search_from..]);
        (clean_text, images)
    }
}

fn guess_mime_type(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
    .to_string()
}

/// 估算文本的 token 数（基于 1 token ≈ 4 字符的近似值）
pub fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    (text.len() as f64 / 4.0).ceil() as u32
}

/// 估算一条消息的 token 数，包括 role overhead 和 tool 字段
pub fn estimate_message_tokens(msg: &Message) -> u32 {
    let mut total = estimate_tokens(&msg.content) + 1; // +1 for role overhead
    if let Some(ref id) = msg.tool_call_id {
        total += estimate_tokens(id);
    }
    if let Some(ref calls) = msg.tool_calls {
        for call in calls {
            total += estimate_tokens(&call.id)
                + estimate_tokens(&call.name)
                + estimate_tokens(&call.arguments);
        }
    }
    total
}

/// 工具定义，用于 LLM function calling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
    pub category: String,              // 工具分类，用于动态工具指南的分组展示
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
