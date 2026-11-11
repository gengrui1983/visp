use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use tracing::Instrument;
use tracing::field;
use visp_config::LlmConfig;
use visp_core::error::LlmError;
use visp_core::message::{Message, MessageType, Role, ToolDefinition};
use visp_core::provider::{ChatEvent, LlmProvider};
use visp_observability::{LangfuseSpanConfig, init_span};

use crate::image_util;
use crate::util::{build_client, parse_retry_after};

/// 检查 tool_call arguments 是否为合法 JSON（空字符串视为合法）。
/// 用于识别 max_tokens 截断导致的不完整 JSON，避免畸形 tool_call 污染会话历史。
fn is_valid_json_args(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// 是否在请求体中携带 `stream_options.include_usage`（默认开启）。
///
/// provider 通过该字段在流末尾返回 token 用量；部分严格的网关/代理会因未知
/// 字段返回 400（如 opencode zen 会插入非标准空 chunk），可通过 extra 关闭：
/// `stream_options = "false"`。
fn stream_include_usage(config: &LlmConfig) -> bool {
    match config.extra.get("stream_options") {
        None => true,
        Some(v) if v.eq_ignore_ascii_case("true") => true,
        Some(v) if v.eq_ignore_ascii_case("false") => false,
        Some(v) => {
            tracing::warn!(
                value = %v,
                "unknown extra `stream_options` value, falling back to true"
            );
            true
        }
    }
}

/// 流式 usage 的累加语义（extra `usage_mode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageMode {
    /// 每个 chunk 的 usage 是从请求开始累计的总量（OpenAI 规范），逐 chunk 差分。
    Cumulative,
    /// 每个 chunk 的 usage 是该 chunk 自身的增量，直接累加。
    Delta,
}

/// 解析 extra `usage_mode`，缺省/未知值回退 `Cumulative`。
fn parse_usage_mode(config: &LlmConfig) -> UsageMode {
    match config.extra.get("usage_mode") {
        None => UsageMode::Cumulative,
        Some(v) if v.eq_ignore_ascii_case("cumulative") => UsageMode::Cumulative,
        Some(v) if v.eq_ignore_ascii_case("delta") => UsageMode::Delta,
        Some(v) => {
            tracing::warn!(
                value = %v,
                "unknown extra `usage_mode` value, falling back to cumulative"
            );
            UsageMode::Cumulative
        }
    }
}

/// 解析 cost 字段：接受 JSON 数字或字符串形式的数字（opencode zen 用字符串）。
fn parse_cost(v: Option<&serde_json::Value>) -> Option<f64> {
    let parsed = match v? {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// 回传历史时的 reasoning 一致性策略。
///
/// 部分网关（如 opencode zen 的 Console Go）在 thinking 模式下要求历史中
/// assistant 消息的 `reasoning_content` 保持一致：要么全部携带、要么全部缺失，
/// 否则间歇性返回 400。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEchoPolicy {
    /// 现行为：assistant 消息按原样透传（有 reasoning_content 就带，没有就不带）。
    Passthrough,
    /// 补齐策略：若同批请求中至少一条 assistant 消息带 `reasoning_content`，
    /// 则为所有缺失该字段的 assistant 消息补 `"reasoning_content": ""`；
    /// 若整批 assistant 都没有该字段，则一条都不加。
    FillMissing,
}

/// 判断 400 响应是否为 reasoning_content 回传一致性错误。
///
/// 命中特征：响应体同时包含 `reasoning_content` 与 `must be passed back`
/// （大小写不敏感、包含匹配即可）。
fn is_reasoning_echo_error(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let lower = body.to_lowercase();
    lower.contains("reasoning_content") && lower.contains("must be passed back")
}

/// 移除所有 assistant 消息的 `reasoning_content` 键（原地修改，不 clone Message）。
fn strip_reasoning_content(messages: &mut [serde_json::Value]) {
    for m in messages.iter_mut() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && let Some(obj) = m.as_object_mut()
        {
            obj.remove("reasoning_content");
        }
    }
}

/// FillMissing 策略的收尾 pass：若至少一条 assistant 消息带 `reasoning_content`，
/// 则为所有缺失该字段的 assistant 消息补空字符串；否则一条都不加。
/// 只影响 assistant 消息，不触碰 system/user/tool。
fn fill_missing_reasoning(messages: &mut [serde_json::Value]) {
    let has_reasoning = messages.iter().any(|m| {
        m.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && m.get("reasoning_content").is_some()
    });
    if !has_reasoning {
        return;
    }
    for m in messages.iter_mut() {
        if m.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && m.get("reasoning_content").is_none()
        {
            m["reasoning_content"] = serde_json::Value::String(String::new());
        }
    }
}

/// 构建 OpenAI API 请求体
pub fn build_openai_request(
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
) -> serde_json::Value {
    build_openai_request_with_policy(messages, tools, config, ReasoningEchoPolicy::Passthrough)
}

/// 按 reasoning 回传策略构建 OpenAI API 请求体。
///
/// 等价于 [`build_openai_request`] + [`build_openai_messages_with_policy`]，
/// 其余字段（model/tools/stream/stream_options/temperature/max_tokens 等）不受策略影响。
pub fn build_openai_request_with_policy(
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
    policy: ReasoningEchoPolicy,
) -> serde_json::Value {
    let openai_messages = build_openai_messages_with_policy(messages, policy);

    let mut request = serde_json::json!({
        "model": config.model,
        "messages": openai_messages,
        "max_tokens": config.max_tokens,
        "temperature": config.temperature,
        "stream": true,
    });

    // stream_options.include_usage：让 provider 在流末尾返回 token 用量
    if stream_include_usage(config) {
        request["stream_options"] = serde_json::json!({"include_usage": true});
    }

    // 添加工具定义（use_tool = false 时不携带）
    if config.use_tool && !tools.is_empty() {
        let openai_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        request["tools"] = serde_json::Value::Array(openai_tools);
    }

    // tool_choice（通过 extra 配置）
    // 支持: "auto" / "none" / "required" / 或 JSON 对象 {"type":"function","function":{"name":"..."}}
    if let Some(tool_choice) = config.extra.get("tool_choice") {
        if tool_choice.starts_with('{') {
            match serde_json::from_str::<serde_json::Value>(tool_choice) {
                Ok(val) => {
                    request["tool_choice"] = val;
                }
                Err(e) => {
                    tracing::warn!("invalid tool_choice JSON {:?}: {e}", tool_choice);
                }
            }
        } else {
            request["tool_choice"] = serde_json::Value::String(tool_choice.clone());
        }
    }

    // 从 extra 配置读取参数
    // response_format
    if let Some(response_format) = config.extra.get("response_format")
        && response_format == "json_object"
    {
        request["response_format"] = serde_json::json!({ "type": "json_object" });
    }

    // seed
    if let Some(seed) = config.extra.get("seed") {
        match seed.parse::<u64>() {
            Ok(n) => {
                request["seed"] = serde_json::json!(n);
            }
            Err(e) => {
                tracing::warn!("invalid seed value {:?}: {e}", seed);
            }
        }
    }

    // frequency_penalty / presence_penalty / top_p
    if let Some(val) = config.extra.get("frequency_penalty") {
        match val.parse::<f64>() {
            Ok(n) => {
                request["frequency_penalty"] = serde_json::json!(n);
            }
            Err(e) => {
                tracing::warn!("invalid frequency_penalty value {:?}: {e}", val);
            }
        }
    }
    if let Some(val) = config.extra.get("presence_penalty") {
        match val.parse::<f64>() {
            Ok(n) => {
                request["presence_penalty"] = serde_json::json!(n);
            }
            Err(e) => {
                tracing::warn!("invalid presence_penalty value {:?}: {e}", val);
            }
        }
    }
    if let Some(val) = config.extra.get("top_p") {
        match val.parse::<f64>() {
            Ok(n) => {
                request["top_p"] = serde_json::json!(n);
            }
            Err(e) => {
                tracing::warn!("invalid top_p value {:?}: {e}", val);
            }
        }
    }

    // enable_thinking（DeepSeek 等模型：true 启用思考，false 关闭）
    if let Some(val) = config.extra.get("enable_thinking") {
        match val.as_str() {
            "true" => {
                request["enable_thinking"] = serde_json::json!(true);
            }
            "false" => {
                request["enable_thinking"] = serde_json::json!(false);
            }
            _ => {
                tracing::warn!("invalid enable_thinking value {:?}", val);
            }
        }
    }

    request
}

/// 构建 OpenAI 兼容的 headers
pub fn build_openai_headers(api_key: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", api_key).parse().unwrap(),
    );
    headers.insert(reqwest::header::USER_AGENT, "visp/0.1.0".parse().unwrap());
    headers
}

/// 将 visp-core Message 转换为 OpenAI Chat API 消息格式
///
/// 规则：
/// - System 角色使用 `role: "system"`
/// - Tool 角色使用 `role: "tool"` + `tool_call_id`
/// - User 消息 content 为字符串
/// - Assistant 消息可包含 content、tool_calls 和 extra_blocks 中的扩展字段
///   （如 thinking，部分 OpenAI 兼容模型支持）
///
/// 等价于 [`build_openai_messages_with_policy`] 使用 [`ReasoningEchoPolicy::Passthrough`]。
pub fn build_openai_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    build_openai_messages_with_policy(messages, ReasoningEchoPolicy::Passthrough)
}

/// 按 reasoning 回传策略构建 OpenAI Chat API 消息格式。
///
/// [`ReasoningEchoPolicy::FillMissing`] 会在收尾 pass 中补齐缺失的
/// `reasoning_content`（详见策略定义），其余行为与 Passthrough 完全一致。
pub fn build_openai_messages_with_policy(
    messages: &[Message],
    policy: ReasoningEchoPolicy,
) -> Vec<serde_json::Value> {
    let mut result = build_openai_messages_raw(messages);
    if policy == ReasoningEchoPolicy::FillMissing {
        fill_missing_reasoning(&mut result);
    }
    result
}

/// 基础消息构建（不含 reasoning 补齐策略）。
fn build_openai_messages_raw(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();
    // 是否已出现过非 system 消息。之后再遇到的 system(上下文裁剪的省略提示、
    // doom-loop 守卫提示等)改为 user 角色原位发送:Qwen 等模型的 chat template
    // 只接受开头的 system,否则 400 "System message must be at the beginning."。
    // 保持原位(而不是像 anthropic 那样合并进 system prompt),既保留提示的位置语义,
    // 也不改变 system prompt 前缀,不破坏服务端前缀缓存。
    let mut seen_non_system = false;

    for msg in messages {
        if msg.role != Role::System {
            seen_non_system = true;
        }
        match msg.role {
            Role::System if seen_non_system => {
                result.push(serde_json::json!({
                    "role": "user",
                    "content": format!("[System notice] {}", msg.content),
                }));
            }
            Role::System => {
                result.push(serde_json::json!({
                    "role": "system",
                    "content": msg.content,
                }));
            }
            Role::User => {
                if msg.images.is_empty() {
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content,
                    }));
                } else {
                    // Multimodal: text + images
                    let mut content: Vec<serde_json::Value> = Vec::new();
                    if !msg.content.is_empty() {
                        content.push(serde_json::json!({
                            "type": "text",
                            "text": msg.content,
                        }));
                    }
                    for img in &msg.images {
                        let data_uri = format!("data:{};base64,{}", img.mime_type, img.base64);
                        content.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": data_uri },
                        }));
                    }
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            Role::Assistant => {
                let content: serde_json::Value = if msg.content.is_empty()
                    && msg.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
                {
                    // OpenAI 规范：纯 tool_calls 消息 content 应为 null
                    serde_json::Value::Null
                } else if msg.kind == MessageType::Thinking {
                    // Thinking-only message: reasoning goes to reasoning_content,
                    // but content must be a non-null string (DeepSeek API requires
                    // content or tool_calls to be set).
                    serde_json::Value::String(String::new())
                } else {
                    serde_json::Value::String(msg.content.clone())
                };
                let mut assistant_msg = serde_json::json!({
                    "role": "assistant",
                    "content": content,
                });

                // 添加 tool_calls（如果有且非空 — OpenAI 拒绝空数组）
                if let Some(ref calls) = msg.tool_calls
                    && !calls.is_empty()
                {
                    let tool_calls: Vec<serde_json::Value> = calls
                        .iter()
                        .map(|tc| {
                            let args = if is_valid_json_args(&tc.arguments) {
                                tc.arguments.clone()
                            } else {
                                "{}".to_string()
                            };
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": args,
                                }
                            })
                        })
                        .collect();
                    assistant_msg["tool_calls"] = serde_json::Value::Array(tool_calls);
                }

                // 合并 extra_blocks（如 thinking）到 assistant message 顶层字段
                // 部分 OpenAI 兼容模型支持这些扩展字段（如 DeepSeek 的 reasoning_content）
                // 跳过 OpenAI 保留字段，避免意外覆盖
                const RESERVED_FIELDS: &[&str] = &["type", "role", "content", "tool_calls"];
                if let Some(ref blocks) = msg.extra_blocks {
                    for block in blocks {
                        if let Some(obj) = block.as_object() {
                            // Check if this is a thinking block and extract the reasoning text
                            let block_type = obj.get("type").and_then(|v| v.as_str());
                            let thinking_text = obj.get("thinking").and_then(|v| v.as_str());
                            if block_type == Some("thinking") {
                                if let Some(text) = thinking_text {
                                    assistant_msg["reasoning_content"] =
                                        serde_json::Value::String(text.to_string());
                                }
                                continue;
                            }
                            // For non-thinking blocks, merge keys as before
                            for (key, val) in obj {
                                if !RESERVED_FIELDS.contains(&key.as_str()) {
                                    assistant_msg[key] = val.clone();
                                }
                            }
                        }
                    }
                }

                result.push(assistant_msg);
            }
            Role::Tool => {
                let tool_call_id = msg.tool_call_id.as_deref().unwrap_or_else(|| {
                    tracing::warn!("Tool message without tool_call_id");
                    ""
                });
                result.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": msg.content,
                }));
            }
        }
    }

    result
}

/// OpenAI SSE 流解析的内部事件
#[derive(Debug)]
pub(crate) enum OpenAiStreamEvent {
    /// 文本增量
    TextDelta(String),
    /// 推理/思考内容增量（部分 OpenAI 兼容模型如 DeepSeek 支持）
    ReasoningDelta(String),
    /// 图片内容块（来自 LLM 输出的 image_url）
    ImageBlock {
        /// base64 数据（不含 `data:` 前缀），远程 URL 时为 None
        data: Option<String>,
        /// MIME 类型（远程 URL 时为空字符串）
        mime_type: String,
        /// 远程 URL（base64 时为 None）
        remote_url: Option<String>,
    },
    /// 工具调用开始（包含 id 和 name）
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// 工具调用参数增量
    ToolCallDelta { index: usize, arguments: String },
    /// 内容块结束，携带 finish_reason
    /// fields 保留用于调试，match 时用 `..` 忽略
    #[allow(dead_code)]
    Finish {
        index: usize,
        reason: Option<String>,
    },
    /// token 用量。
    /// `cost` 来自部分网关（如 opencode zen）的 usage 对象或流末尾非标准帧
    /// `{"choices":[],"cost":"0.0012"}`。
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        cost: Option<f64>,
    },
    /// 流结束标记 `[DONE]`
    StreamEnd,
    /// 跳过（忽略事件）
    Skip,
}

/// 解析单行 OpenAI SSE data（不含 `data: ` 前缀）
///
/// 返回事件列表：常规情况下单元素；`delta.content` 为数组时（OpenAI
/// 图片输出格式）可能返回多个事件（文本 + 图片）。
///
/// OpenAI SSE 格式：
/// ```text
/// data: {"id":"...","object":"chat.completion.chunk","choices":[{"delta":{"content":"Hello"},"index":0}]}
/// data: {"id":"...","object":"chat.completion.chunk","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]},"index":0}]}
/// data: [DONE]
/// ```
pub(crate) fn parse_openai_sse_data(data: &str) -> Result<Vec<OpenAiStreamEvent>, LlmError> {
    if data == "[DONE]" {
        return Ok(vec![OpenAiStreamEvent::StreamEnd]);
    }
    if data.trim().is_empty() {
        return Ok(vec![OpenAiStreamEvent::Skip]);
    }

    let v: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| LlmError::Stream(format!("parse openai data: {e}")))?;

    // usage 解析：token 用量在 `usage` 对象内（OpenAI 规范），cost 可能出现在
    // usage 对象内，也可能以**帧顶层** `cost` 字段出现在非标准帧中
    // （opencode zen 会在流末尾追加 `{"choices":[],"cost":"0"}`）。
    // 注意：OpenAI 官方约定只有最后一个 chunk 带 usage 且 choices 为空，但部分
    // 兼容网关（如 opencode zen）在每个 chunk 都附带 usage。因此这里先解析
    // 暂存，不让 usage 分支拦截同 chunk 中 delta 内容的解析。
    // 注意 2：usage 的累加语义（累计总量 / 单 chunk 增量）由 `extra.usage_mode`
    // 决定，在状态机中处理，这里只做字段提取。
    let mut usage_events: Vec<OpenAiStreamEvent> = Vec::new();
    let mut cost = parse_cost(v.get("cost"));
    if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
        let input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0) as u32;
        // Try Anthropic-style cache fields first (some OpenAI-compatible providers use them)
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        // OpenAI standard: usage.prompt_tokens_details.cached_tokens
        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        // Also try Anthropic-style field as fallback
        let cache_read = if cache_read == 0 {
            usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32
        } else {
            cache_read
        };
        if cost.is_none() {
            cost = parse_cost(usage.get("cost"));
        }
        if input_tokens > 0 || output_tokens > 0 || cost.is_some() {
            usage_events.push(OpenAiStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
                cost,
            });
        }
    } else if let Some(c) = cost {
        // 非标准尾帧：只有顶层 cost，没有 usage 对象
        usage_events.push(OpenAiStreamEvent::Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost: Some(c),
        });
    }

    // 解析 choices
    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            // 无内容 chunk：仅 usage（官方约定的最终 usage chunk）或 Skip
            return Ok(if usage_events.is_empty() {
                vec![OpenAiStreamEvent::Skip]
            } else {
                usage_events
            });
        }
    };

    let choice = &choices[0];
    let delta = &choice["delta"];
    let finish_reason = choice["finish_reason"].as_str().map(|s| s.to_string());
    let index = choice["index"].as_u64().unwrap_or(0) as usize;

    // 按 chunk 收集事件：同一个 delta 中可能同时存在 content / reasoning /
    // tool_calls / 旧式 function_call，必须各自 push 而非提前 return。
    // 语义约定：所有 delta 字段处理完毕后统一追加 usage，最后返回一次。
    let mut events: Vec<OpenAiStreamEvent> = Vec::new();

    // content（可以是字符串或数组：text + image_url）
    if let Some(content) = delta.get("content") {
        // 字符串形式（常规流式文本）
        if let Some(text) = content.as_str()
            && !text.is_empty()
        {
            events.push(OpenAiStreamEvent::TextDelta(text.to_string()));
        } else if let Some(items) = content.as_array() {
            // 数组形式（OpenAI 图片输出：text + image_url 块）
            let mut text_parts: Vec<String> = Vec::new();
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("image_url") => {
                        let url = item
                            .get("image_url")
                            .and_then(|u| u.get("url"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        if url.starts_with("data:") {
                            // data URI：提取 mime + base64，由调用方保存到磁盘
                            if let Some((mime, base64_data)) = image_util::parse_data_uri(url) {
                                events.push(OpenAiStreamEvent::ImageBlock {
                                    data: Some(base64_data),
                                    mime_type: mime,
                                    remote_url: None,
                                });
                            }
                        } else if url.starts_with("http://") || url.starts_with("https://") {
                            // 远程 URL：不下载，透传给上层
                            events.push(OpenAiStreamEvent::ImageBlock {
                                data: None,
                                mime_type: String::new(),
                                remote_url: Some(url.to_string()),
                            });
                        }
                    }
                    _ => {}
                }
            }
            if !text_parts.is_empty() {
                // 文本块排在图片事件之前，保持既有事件顺序
                events.insert(0, OpenAiStreamEvent::TextDelta(text_parts.join("")));
            }
        }
    }

    // reasoning_content（DeepSeek 等模型发送推理内容）
    if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str())
        && !reasoning.is_empty()
    {
        events.push(OpenAiStreamEvent::ReasoningDelta(reasoning.to_string()));
    }
    // 部分提供商使用 "reasoning" 字段
    if let Some(reasoning) = delta.get("reasoning").and_then(|c| c.as_str())
        && !reasoning.is_empty()
    {
        events.push(OpenAiStreamEvent::ReasoningDelta(reasoning.to_string()));
    }

    // tool_calls delta：数组内每个元素都要处理（不得只取第一个）
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let tc_index = tc["index"].as_u64().unwrap_or(0) as usize;
            let func = &tc["function"];

            // 有非空 id 表示新的工具调用开始
            if let Some(id) = tc.get("id").and_then(|i| i.as_str())
                && !id.is_empty()
            {
                let name = func["name"].as_str().unwrap_or("").to_string();
                events.push(OpenAiStreamEvent::ToolCallStart {
                    index: tc_index,
                    id: id.to_string(),
                    name,
                });
            }

            // 参数增量（可能与 id 同帧出现，此时两者都产出）
            if let Some(arguments) = func.get("arguments").and_then(|a| a.as_str())
                && !arguments.is_empty()
            {
                events.push(OpenAiStreamEvent::ToolCallDelta {
                    index: tc_index,
                    arguments: arguments.to_string(),
                });
            }
        }
    }

    // 旧式 function_call（无 id）：首帧给 name，后续帧给 arguments。
    // 合成一个稳定 id，使状态机可正常累积与发射。
    if let Some(fc) = delta.get("function_call") {
        if let Some(name) = fc.get("name").and_then(|n| n.as_str())
            && !name.is_empty()
        {
            events.push(OpenAiStreamEvent::ToolCallStart {
                index: 0,
                id: "call_legacy_0".to_string(),
                name: name.to_string(),
            });
        }
        if let Some(arguments) = fc.get("arguments").and_then(|a| a.as_str())
            && !arguments.is_empty()
        {
            events.push(OpenAiStreamEvent::ToolCallDelta {
                index: 0,
                arguments: arguments.to_string(),
            });
        }
    }

    // finish_reason 最后处理
    if let Some(reason) = finish_reason {
        events.push(OpenAiStreamEvent::Finish {
            index,
            reason: Some(reason),
        });
    }

    // 统一追加 usage 事件并返回一次
    if events.is_empty() {
        return Ok(if usage_events.is_empty() {
            vec![OpenAiStreamEvent::Skip]
        } else {
            usage_events
        });
    }
    events.extend(usage_events);
    Ok(events)
}

/// 按字符边界安全截取，最多取 `max_chars` 个字符。
///
/// 用于日志预览，避免在 UTF-8 多字节字符（如中文）中间切片导致 panic。
/// 返回切片，不分配新内存。
fn truncate_for_log(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// 将 OpenAI SSE 字节流转换为 ChatEvent 流（默认 usage 累计语义）
///
/// 仅供测试使用的便捷入口；生产路径请调用 [`byte_stream_to_chat_events_with_mode`]，
/// 由 `extra.usage_mode` 决定 usage 累加语义。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn byte_stream_to_chat_events(
    byte_stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    start_time: std::time::Instant,
    span: tracing::Span,
    request_model: String,
    project_path: String,
    langfuse_capture_output: bool,
    langfuse_capture_max_chars: usize,
    langfuse_redact_secrets: bool,
) -> Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>> {
    byte_stream_to_chat_events_with_mode(
        byte_stream,
        start_time,
        span,
        request_model,
        project_path,
        langfuse_capture_output,
        langfuse_capture_max_chars,
        langfuse_redact_secrets,
        UsageMode::Cumulative,
    )
}

/// 将 OpenAI SSE 字节流转换为 ChatEvent 流
///
/// OpenAI 的 SSE 格式简单：
/// - 每条消息以 `data: ` 开头，空行分隔
/// - 流结束标记为 `data: [DONE]`
/// - 每个 data 行可能包含 usage、choices 中的 text delta 或 tool calls delta
///
/// `usage_mode` 决定 chunk 内 usage 的解释方式（累计总量 vs 单 chunk 增量）。
#[allow(clippy::too_many_arguments)]
fn byte_stream_to_chat_events_with_mode(
    byte_stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    start_time: std::time::Instant,
    span: tracing::Span,
    request_model: String,
    project_path: String,
    langfuse_capture_output: bool,
    langfuse_capture_max_chars: usize,
    langfuse_redact_secrets: bool,
    usage_mode: UsageMode,
) -> Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>> {
    struct StreamState {
        stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
        buf: String,
        /// 跨 chunk 的不完整 UTF-8 尾字节缓冲（修复多字节字符被 TCP chunk 切坏的问题）
        pending_bytes: Vec<u8>,
        /// 累积中的工具调用: index -> (id, name, arguments)
        tool_acc: HashMap<usize, (String, String, String)>,
        /// 待发射的工具调用列表: (id, name, arguments)
        pending_tool_calls: Vec<(String, String, String)>,
        /// 同一条 SSE 行产生的多个事件缓存（逐条发射）
        pending_events: VecDeque<OpenAiStreamEvent>,
        /// 图片保存目录（base64 图片落盘的项目路径）
        project_path: String,
        /// 已完成的工具调用数量（用于 UsageInfo）
        tool_call_count: u32,
        /// 累积的推理/思考内容（来自 reasoning_content / reasoning 字段）
        reasoning_text: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// usage 累加语义（累计总量 / 单 chunk 增量），来自 `extra.usage_mode`
        usage_mode: UsageMode,
        /// 累积成本（provider 提供时才有值，如 opencode zen 的 cost 字段）
        cost: Option<f64>,
        /// 响应模型名称（从 chunk 顶层 model 字段提取）
        model: String,
        /// 请求时的模型名称（用于 response model 为空时的 fallback）
        request_model: String,
        /// 结束原因（从 Finish 事件提取）
        finish_reason: String,
        /// 请求开始时刻（用于计算端到端 latency）
        state_start_time: std::time::Instant,
        /// 标记流是否已结束
        stream_ended: bool,
        /// 是否已发射 UsageInfo
        usage_emitted: bool,
        /// 是否已发射 OutputMetadata
        metadata_emitted: bool,
        /// 是否已发射 Done
        done_emitted: bool,
        /// gen_ai.client.operation span
        span: tracing::Span,
        /// 是否已发射首 token 事件
        first_content_emitted: bool,
        /// Langfuse 输出捕获配置
        langfuse_capture_output: bool,
        langfuse_capture_max_chars: usize,
        langfuse_redact_secrets: bool,
        /// 累积的输出文本（用于 langfuse.observation.output）
        accumulated_output: String,
        /// 本轮是否已发射过 TextDelta（用于判断截断时是否需补提示文本）
        had_text_output: bool,
        /// 待发射的截断提示文本（finish_reason='length' 丢弃畸形 tool_call 且无文本时设置）
        pending_hint: Option<String>,
    }

    /// 将累积的工具调用（tool_acc）转移到 pending_tool_calls。
    /// finish_reason='length' 截断的 tool_call arguments 可能是不完整 JSON，
    /// 转移时校验合法性，丢弃畸形的，避免污染会话历史导致后续请求 400。
    fn flush_tool_acc(state: &mut StreamState) {
        if !state.tool_acc.is_empty() {
            let calls: Vec<(String, String, String)> = state
                .tool_acc
                .drain()
                .filter(|(_, (_, _, args))| is_valid_json_args(args))
                .map(|(_, (id, name, args))| (id, name, args))
                .collect();
            state.tool_call_count = calls.len() as u32;
            state.pending_tool_calls = calls.into_iter().rev().collect();
        }
    }

    /// 标记首 token 已到达（幂等，仅首次有效）
    fn emit_first_token(state: &mut StreamState) {
        if !state.first_content_emitted {
            state.first_content_emitted = true;
            tracing::debug!(target: "gen_ai.client.first_token", "first token received");
        }
    }

    /// 处理单个 OpenAiStreamEvent，返回需要发射的 ChatEvent（仅更新内部状态则返回 None）
    fn handle_stream_event(state: &mut StreamState, event: OpenAiStreamEvent) -> Option<ChatEvent> {
        match event {
            OpenAiStreamEvent::TextDelta(text) => {
                if state.langfuse_capture_output {
                    state.accumulated_output.push_str(&text);
                }
                state.had_text_output = true;
                emit_first_token(state);
                Some(ChatEvent::TextDelta(text))
            }
            OpenAiStreamEvent::ReasoningDelta(text) => {
                state.reasoning_text.push_str(&text);
                // 立即发射 ThinkingBlock 实现流式显示（不等待 text delta）
                let block = serde_json::json!({
                    "type": "thinking",
                    "thinking": state.reasoning_text.clone(),
                });
                emit_first_token(state);
                Some(ChatEvent::ThinkingBlock(block))
            }
            OpenAiStreamEvent::ImageBlock {
                data,
                mime_type,
                remote_url,
            } => {
                emit_first_token(state);
                if let Some(data) = data {
                    // base64 图片：保存到磁盘，失败时发射 ImageError
                    match image_util::save_base64_image(&data, &mime_type, &state.project_path) {
                        Ok(path) => Some(ChatEvent::ImageBlock {
                            path,
                            mime_type,
                            remote_url: None,
                        }),
                        Err(e) => Some(ChatEvent::ImageError {
                            reason: e.to_string(),
                        }),
                    }
                } else {
                    // 远程 URL 图片：不下载，直接透传
                    remote_url.map(|remote_url| ChatEvent::ImageBlock {
                        path: String::new(),
                        mime_type: String::new(),
                        remote_url: Some(remote_url),
                    })
                }
            }
            OpenAiStreamEvent::ToolCallStart { index, id, name } => {
                tracing::debug!(index, %id, %name, "ToolCallStart");
                state.tool_acc.insert(index, (id, name, String::new()));
                None
            }
            OpenAiStreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(entry) = state.tool_acc.get_mut(&index) {
                    entry.2.push_str(&arguments);
                } else {
                    tracing::warn!(
                        index,
                        delta_len = arguments.len(),
                        "ToolCallDelta for unknown tool index (ToolCallStart not received?)"
                    );
                }
                None
            }
            OpenAiStreamEvent::Finish { reason, .. } => {
                if let Some(ref r) = reason {
                    state.finish_reason = r.clone();
                    tracing::debug!(
                        finish_reason = %r,
                        had_text_output = state.had_text_output,
                        reasoning_len = state.reasoning_text.len(),
                        tool_acc_count = state.tool_acc.len(),
                        "OpenAI stream: received finish_reason"
                    );
                    if r == "content_filter" {
                        tracing::warn!("OpenAI response blocked by content filter");
                    }
                    if r == "length" && !state.tool_acc.is_empty() {
                        tracing::warn!(
                            tool_count = state.tool_acc.len(),
                            "finish_reason='length' — tool call arguments likely truncated (max_output_tokens exceeded)"
                        );
                    }
                }
                // 转移 tool_acc（校验丢弃畸形 arguments）
                let had_tool_acc = !state.tool_acc.is_empty();
                flush_tool_acc(state);
                // finish_reason='length' 且丢弃后无 tool_call 且本轮无文本：
                // 补一条提示文本，保证 assistant 消息非空且语义可追踪
                if state.finish_reason == "length"
                    && had_tool_acc
                    && state.pending_tool_calls.is_empty()
                    && !state.had_text_output
                {
                    state.pending_hint = Some(
                        "[tool call truncated due to max_tokens limit, please retry]".to_string(),
                    );
                }
                // 收到 finish_reason 即标记流结束，无需等待底层流关闭
                state.stream_ended = true;
                None
            }
            OpenAiStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                cost,
            } => {
                // 依 usage_mode 解释本次 usage：cumulative 模式下是「从请求开始的
                // 累计总量」，需与上一轮求差分；delta 模式下本身就是增量。
                let (delta_in, delta_out, delta_cc, delta_cr) = match state.usage_mode {
                    UsageMode::Cumulative => {
                        let d = (
                            input_tokens.saturating_sub(state.input_tokens),
                            output_tokens.saturating_sub(state.output_tokens),
                            cache_creation_input_tokens
                                .saturating_sub(state.cache_creation_input_tokens),
                            cache_read_input_tokens.saturating_sub(state.cache_read_input_tokens),
                        );
                        // 状态里保存累计值（可回退的场景也只在增大时覆盖）
                        if input_tokens > 0 {
                            state.input_tokens = input_tokens;
                        }
                        if output_tokens > 0 {
                            state.output_tokens = output_tokens;
                        }
                        if cache_creation_input_tokens > 0 {
                            state.cache_creation_input_tokens = cache_creation_input_tokens;
                        }
                        if cache_read_input_tokens > 0 {
                            state.cache_read_input_tokens = cache_read_input_tokens;
                        }
                        d
                    }
                    UsageMode::Delta => {
                        // 增量语义：累加到总量（修复此前「后值覆盖前值」导致
                        // 增量型网关总量偏小的 bug）
                        state.input_tokens = state.input_tokens.saturating_add(input_tokens);
                        state.output_tokens = state.output_tokens.saturating_add(output_tokens);
                        state.cache_creation_input_tokens = state
                            .cache_creation_input_tokens
                            .saturating_add(cache_creation_input_tokens);
                        state.cache_read_input_tokens = state
                            .cache_read_input_tokens
                            .saturating_add(cache_read_input_tokens);
                        (
                            input_tokens,
                            output_tokens,
                            cache_creation_input_tokens,
                            cache_read_input_tokens,
                        )
                    }
                };

                if let Some(c) = cost {
                    state.cost = Some(match (state.usage_mode, state.cost) {
                        // 累计语义：后到的值就是新的累计值
                        (UsageMode::Cumulative, _) => c,
                        // 增量语义：逐次累加
                        (UsageMode::Delta, Some(prev)) => prev + c,
                        (UsageMode::Delta, None) => c,
                    });
                }

                // 逐 chunk 上报增量（供 CLI 实时速率显示；最终总量仍以流末
                // UsageInfo 为准，因此调用方不应把它累加进结算统计）
                if delta_in > 0 || delta_out > 0 || delta_cc > 0 || delta_cr > 0 {
                    return Some(ChatEvent::UsageDelta {
                        input_tokens: delta_in,
                        output_tokens: delta_out,
                        cache_creation_input_tokens: delta_cc,
                        cache_read_input_tokens: delta_cr,
                    });
                }
                None
            }
            OpenAiStreamEvent::Skip => None,
            OpenAiStreamEvent::StreamEnd => {
                state.stream_ended = true;
                flush_tool_acc(state);
                None
            }
        }
    }

    let state = StreamState {
        stream: Box::pin(byte_stream),
        buf: String::new(),
        pending_bytes: Vec::new(),
        tool_acc: HashMap::new(),
        pending_tool_calls: Vec::new(),
        pending_events: VecDeque::new(),
        project_path,
        tool_call_count: 0,
        reasoning_text: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        usage_mode,
        cost: None,
        model: String::new(),
        request_model,
        finish_reason: String::new(),
        state_start_time: start_time,
        stream_ended: false,
        usage_emitted: false,
        metadata_emitted: false,
        done_emitted: false,
        span,
        first_content_emitted: false,
        langfuse_capture_output,
        langfuse_capture_max_chars,
        langfuse_redact_secrets,
        accumulated_output: String::new(),
        had_text_output: false,
        pending_hint: None,
    };

    let event_stream = stream::unfold(state, |mut state| {
        let span = state.span.clone();
        async move {
            loop {
                // [A0] 发射缓存的待处理事件（一条 SSE 行可能产生多个事件）
                if let Some(event) = state.pending_events.pop_front() {
                    if let Some(chat_event) = handle_stream_event(&mut state, event) {
                        return Some((Ok(chat_event), state));
                    }
                    continue;
                }

                // [A] 优先处理缓冲区中完整的 SSE 消息（一次一条）
                if let Some(pos) = state.buf.find("\n\n") {
                    let raw = state.buf[..pos].to_string();
                    state.buf = state.buf[pos + 2..].to_string();

                    for line in raw.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            tracing::trace!(
                                target: "visp.sse.openai",
                                data,
                                "raw SSE data line"
                            );
                            // 从 chunk 顶层提取 model 字段
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
                                && let Some(m) = v.get("model").and_then(|m| m.as_str())
                                && !m.is_empty()
                            {
                                state.model = m.to_string();
                            }
                            // 解析为事件列表；多个事件缓存到 pending_events 逐条发射
                            match parse_openai_sse_data(data) {
                                Ok(events) => {
                                    state.pending_events.extend(events);
                                }
                                Err(e) => {
                                    return Some((Err(e), state));
                                }
                            }
                        } else if !line.trim().is_empty() {
                            tracing::trace!(?line, "ignored non-data SSE line");
                        }
                    }

                    // 继续处理下一条消息
                    continue;
                }

                // [B] 发射待处理的工具调用
                if let Some((id, name, args)) = state.pending_tool_calls.pop() {
                    emit_first_token(&mut state);
                    tracing::debug!(
                        %name,
                        args_len = args.len(),
                        args_preview = %truncate_for_log(&args, 200),
                        "emitting ToolCall"
                    );
                    return Some((
                        Ok(ChatEvent::ToolCall {
                            id,
                            name,
                            arguments: args,
                        }),
                        state,
                    ));
                }

                // [B'] 发射截断提示文本（finish_reason='length' 丢弃畸形 tool_call 且无文本时）
                if let Some(hint) = state.pending_hint.take() {
                    emit_first_token(&mut state);
                    return Some((Ok(ChatEvent::TextDelta(hint)), state));
                }

                // [C] 处理流结束标记
                if state.stream_ended {
                    if !state.usage_emitted {
                        state.usage_emitted = true;
                        return Some((
                            Ok(ChatEvent::UsageInfo {
                                input_tokens: state.input_tokens,
                                output_tokens: state.output_tokens,
                                tool_calls: state.tool_call_count,
                                cache_creation_input_tokens: state.cache_creation_input_tokens,
                                cache_read_input_tokens: state.cache_read_input_tokens,
                                cost: state.cost,
                            }),
                            state,
                        ));
                    }
                    if !state.metadata_emitted {
                        state.metadata_emitted = true;
                        let latency = state.state_start_time.elapsed().as_millis() as u64;
                        let finish_reasons = if state.finish_reason.is_empty() {
                            vec![]
                        } else {
                            vec![state.finish_reason.clone()]
                        };

                        // 在 span 上 record usage / model / finish_reasons / cost
                        // Cast u32 → i64: tracing-opentelemetry's Visit impl only handles
                        // record_i64 (not record_u64), so u32/u64 values would fall through
                        // to record_debug and be exported as String("100") instead of I64(100).
                        state
                            .span
                            .record("gen_ai.usage.input_tokens", state.input_tokens as i64);
                        state
                            .span
                            .record("gen_ai.usage.output_tokens", state.output_tokens as i64);
                        if let Some(cost) = state.cost {
                            state.span.record("gen_ai.usage.cost", cost);
                        }
                        // OpenAI 不写 cache 字段
                        if state.finish_reason == "length" {
                            state.span.record("visp.llm.token_limit_hit", true);
                        }
                        let finish_reasons_str =
                            serde_json::to_string(&finish_reasons).unwrap_or_default();
                        state
                            .span
                            .record("gen_ai.response.finish_reasons", &finish_reasons_str);
                        // Fallback to request model if response didn't include one
                        // (some OpenAI-compatible providers don't include "model" in SSE chunks)
                        let effective_model = if state.model.is_empty() {
                            &state.request_model
                        } else {
                            &state.model
                        };
                        state.span.record("gen_ai.response.model", effective_model);

                        // Langfuse generation capture: record output if enabled
                        let raw_output_len = state.accumulated_output.len();
                        if state.langfuse_capture_output && raw_output_len > 0 {
                            let sanitized = visp_core::sanitize::format_langfuse_output(
                                &state.accumulated_output,
                                state.langfuse_capture_max_chars,
                                state.langfuse_redact_secrets,
                            );
                            state.span.record("langfuse.observation.output", &sanitized);
                        }

                        return Some((
                            Ok(ChatEvent::OutputMetadata(visp_core::ProviderMetadata {
                                model: effective_model.clone(),
                                finish_reasons,
                                input_tokens: state.input_tokens,
                                output_tokens: state.output_tokens,
                                cache_read_input_tokens: if state.cache_read_input_tokens > 0 {
                                    Some(state.cache_read_input_tokens)
                                } else {
                                    None
                                },
                                cache_creation_input_tokens: if state.cache_creation_input_tokens
                                    > 0
                                {
                                    Some(state.cache_creation_input_tokens)
                                } else {
                                    None
                                },
                                latency_ms: latency,
                            })),
                            state,
                        ));
                    }
                    if !state.done_emitted {
                        state.done_emitted = true;
                        tracing::debug!(
                            target: "gen_ai.client.completed",
                            input_tokens = state.input_tokens,
                            output_tokens = state.output_tokens,
                            model = %state.model,
                            "LLM request completed"
                        );
                        return Some((Ok(ChatEvent::Done), state));
                    }
                    return None;
                }

                // [D] 从底层流读取更多数据
                match state.stream.next().await {
                    Some(Ok(chunk)) => {
                        state.pending_bytes.extend_from_slice(&chunk);
                        // 只解码完整的 UTF-8 部分，不完整的尾字节留到下一个 chunk
                        let safe_end = match std::str::from_utf8(&state.pending_bytes) {
                            Ok(_) => state.pending_bytes.len(),
                            Err(e) => e.valid_up_to(),
                        };
                        if safe_end > 0 {
                            state.buf.push_str(&String::from_utf8_lossy(
                                &state.pending_bytes[..safe_end],
                            ));
                            state.pending_bytes = state.pending_bytes[safe_end..].to_vec();
                        }
                    }
                    Some(Err(e)) => {
                        return Some((Err(LlmError::Network(e.to_string())), state));
                    }
                    None => {
                        tracing::debug!(
                            model = %state.model,
                            finish_reason = %state.finish_reason,
                            had_text_output = state.had_text_output,
                            reasoning_len = state.reasoning_text.len(),
                            tool_acc_count = state.tool_acc.len(),
                            "OpenAI stream: byte stream ended (None)"
                        );
                        // 流自然结束
                        state.stream_ended = true;
                        flush_tool_acc(&mut state);
                    }
                }
            }
        }
        .instrument(span)
    });

    Box::pin(event_stream)
}

/// 检查 base_url 是否已经包含版本路径段（如 /v1, /v3 等）。
/// 如果用户提供了带版本的自定义 base_url（例如火山引擎 Ark 的 /api/plan/v3），
/// 则不重复追加 /v1 前缀。
fn is_versioned_base_url(url: &str) -> bool {
    url.rsplit('/')
        .next()
        .is_some_and(|seg| seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit()))
}

/// OpenAI API 提供器
pub struct OpenAiProvider {
    api_key: String,
    api_url: String,
    client: reqwest::Client,
    /// 附加请求头（如 opencode 网关的 x-opencode-session），随每个请求发送。
    /// 值中的 {session} 占位符会被 LlmConfig.session_id 替换。
    extra_headers: Vec<(String, String)>,
    /// 回传历史时的 reasoning 一致性策略（默认 Passthrough）。
    reasoning_policy: ReasoningEchoPolicy,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            api_url: "https://api.openai.com".to_string(),
            client: build_client(),
            extra_headers: Vec::new(),
            reasoning_policy: ReasoningEchoPolicy::Passthrough,
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            api_url: base_url,
            client: build_client(),
            extra_headers: Vec::new(),
            reasoning_policy: ReasoningEchoPolicy::Passthrough,
        }
    }

    /// 设置附加请求头（如 opencode 的 x-opencode-session）。
    /// 值中的 `{session}` 占位符在请求时替换为 LlmConfig.session_id。
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// 设置回传历史时的 reasoning 一致性策略（默认 Passthrough）。
    pub fn with_reasoning_policy(mut self, policy: ReasoningEchoPolicy) -> Self {
        self.reasoning_policy = policy;
        self
    }

    /// 当前 reasoning 回传策略（只读访问器）。
    pub fn reasoning_policy(&self) -> ReasoningEchoPolicy {
        self.reasoning_policy
    }

    /// 将 extra_headers 应用到请求头，`{session}` 占位符替换为 config.session_id。
    /// 未设置 session_id 时含占位符的头跳过。
    fn apply_extra_headers(&self, headers: &mut reqwest::header::HeaderMap, config: &LlmConfig) {
        for (name, value) in &self.extra_headers {
            let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
                tracing::warn!(header = %name, "invalid extra header name, skipping");
                continue;
            };
            let resolved = if value.contains("{session}") {
                let Some(sid) = config.session_id.as_deref() else {
                    continue;
                };
                value.replace("{session}", sid)
            } else {
                value.clone()
            };
            match reqwest::header::HeaderValue::from_str(&resolved) {
                Ok(v) => {
                    headers.insert(name, v);
                }
                Err(e) => {
                    tracing::warn!(header = %name, error = %e, "invalid extra header value, skipping");
                }
            }
        }
    }

    /// 调用文生图 API（/images/generations），返回 ImageBlock 事件流。
    ///
    /// 从 messages 中提取最后一条 user 消息作为 prompt，
    /// 发送非流式请求，将返回的图片 URL 包装为 ChatEvent::ImageBlock。
    async fn image_generate(
        &self,
        messages: &[Message],
        config: &LlmConfig,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        use visp_core::message::Role;

        // 1. 从 messages 提取最后一条 user 消息作为 prompt
        let prompt: String = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .ok_or_else(|| LlmError::Api {
                status: 400,
                message: "No user message found for image generation prompt".to_string(),
            })?;

        // 2. 构建请求体与 URL（OpenAI 兼容 /images/generations）
        let mut body = serde_json::json!({
            "model": config.model,
            "prompt": prompt,
            "response_format": "url",
        });
        // 从 config.extra 透传可选参数
        for key in &["size", "output_format", "watermark"] {
            if let Some(val) = config.extra.get(*key) {
                body[key] = serde_json::Value::String(val.clone());
            }
        }
        let base = self.api_url.trim_end_matches('/');
        let url = if is_versioned_base_url(base) {
            format!("{base}/images/generations")
        } else {
            format!("{base}/v1/images/generations")
        };

        // 3. 构建请求头
        let mut headers = build_openai_headers(&self.api_key);
        self.apply_extra_headers(&mut headers, config);

        // 6. 发送请求
        tracing::debug!(url = %url, model = %config.model, "image generation request");
        let send_fut = self.client.post(&url).headers(headers).json(&body).send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            resp = send_fut => resp.map_err(|e| LlmError::Network(e.to_string()))?,
        };

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                message: format!("Image generation API error: {}", body_text),
            });
        }

        // 7. 解析响应 JSON
        let resp_json: serde_json::Value = response.json().await.map_err(|e| LlmError::Api {
            status: 502,
            message: format!("Failed to parse image generation response: {}", e),
        })?;

        let image_url = resp_json
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|item| item.get("url"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| LlmError::Api {
                status: 502,
                message: format!("No image URL in response: {}", resp_json),
            })?
            .to_string();

        // 8. 构建事件流：ImageBlock + Done
        let events = vec![
            Ok(ChatEvent::ImageBlock {
                path: String::new(),
                mime_type: String::new(),
                remote_url: Some(image_url),
            }),
            Ok(ChatEvent::Done),
        ];

        Ok(Box::pin(stream::iter(events)))
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        config: &LlmConfig,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        // Provider name from config (falls back to "openai" for backward compat)
        let provider_name = config.provider.as_deref().unwrap_or("openai");
        // 创建 gen_ai.client.operation span
        let lf_cfg = LangfuseSpanConfig::from(config);
        let span = init_span!(
            &lf_cfg,
            config.langfuse_session_id.as_deref().unwrap_or(""),
            config.langfuse_trace_name.as_deref().unwrap_or(""),
            "gen_ai.client.operation",
            gen_ai.system = "openai",
            gen_ai.request.model = %config.model,
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = %provider_name,
            gen_ai.request.max_tokens = config.max_tokens as i64,
            gen_ai.request.temperature = config.temperature,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = field::Empty,
            gen_ai.usage.output_tokens = field::Empty,
            gen_ai.response.finish_reasons = field::Empty,
            gen_ai.response.model = field::Empty,
            visp.llm.token_limit_hit = field::Empty,
            visp.tools.definitions = field::Empty,
            gen_ai.client.base_url = %self.api_url,
        );

        // 文生图模型：走 /images/generations 端点
        if config.image_generation {
            return self.image_generate(messages, config, cancel).await;
        }

        let base = self.api_url.trim_end_matches('/');
        let url = if is_versioned_base_url(base) {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        };
        let mut body =
            build_openai_request_with_policy(messages, tools, config, self.reasoning_policy);
        let mut headers = build_openai_headers(&self.api_key);
        self.apply_extra_headers(&mut headers, config);

        let capture_enabled = config.langfuse_capture_input || config.langfuse_capture_output;
        if capture_enabled {
            span.record("langfuse.observation.type", "generation");
        }
        if config.langfuse_capture_input {
            // Record only the messages array, not the full request body
            let input = body.get("messages").unwrap_or(&body);
            let sanitized = visp_core::sanitize::format_langfuse_input(
                input,
                config.langfuse_capture_max_chars,
                config.langfuse_redact_secrets,
            );
            span.record("langfuse.observation.input", &sanitized);

            // Record tools as a separate attribute
            if let Some(tools_val) = body.get("tools") {
                let tools_str = serde_json::to_string(tools_val).unwrap_or_default();
                let tools_sanitized = visp_core::sanitize::sanitize_and_truncate(
                    &tools_str,
                    config.langfuse_capture_max_chars,
                    config.langfuse_redact_secrets,
                );
                span.record("visp.tools.definitions", &tools_sanitized);
            }
        }

        tracing::debug!(url = %url, model = %config.model, "OpenAI request");
        let start_time = std::time::Instant::now();
        let send_fut = self.client.post(&url).headers(headers).json(&body).send();
        let mut response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            resp = send_fut => resp.map_err(|e| LlmError::Network(e.to_string()))?,
        };

        let mut status = response.status();
        // 400 reasoning_content 回传一致性错误（如 opencode zen Console Go 的
        // "must be passed back"）：降级重试一次——剥离所有 assistant 消息的
        // reasoning_content 后重发（不再应用 FillMissing）。只允许一次，用局部
        // bool 标记，禁止递归/循环。
        let mut degraded = false;
        if status == reqwest::StatusCode::BAD_REQUEST {
            let body_text = response.text().await.unwrap_or_default();
            if is_reasoning_echo_error(status.as_u16(), &body_text) {
                degraded = true;
                tracing::warn!(
                    model = %config.model,
                    session_id = ?config.session_id,
                    error = %body_text,
                    "400 reasoning_content echo error, retrying once with reasoning_content stripped"
                );
                // 降级请求体：移除所有 assistant 消息的 reasoning_content，其余字段不变
                if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                    strip_reasoning_content(messages);
                }
                let mut retry_headers = build_openai_headers(&self.api_key);
                self.apply_extra_headers(&mut retry_headers, config);
                let send_fut = self
                    .client
                    .post(&url)
                    .headers(retry_headers)
                    .json(&body)
                    .send();
                response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                    resp = send_fut => resp.map_err(|e| LlmError::Network(e.to_string()))?,
                };
                status = response.status();
            } else {
                // 非匹配 400：沿用统一错误分支的行为（发 gen_ai.client.error 观测事件 +
                // 同样的错误值），避免因提前 return 丢事件、与改动前行为不一致。
                span.in_scope(|| {
                    tracing::error!(target: "gen_ai.client.error", error_type = "api_error", status = status.as_u16(), degraded, "API error");
                });
                return Err(LlmError::Api {
                    status: status.as_u16(),
                    message: body_text,
                });
            }
        }

        if status.is_success() {
            let byte_stream = response.bytes_stream();
            // 图片保存目录：优先取 config.extra 的 project_path，缺省用系统临时目录
            let project_path = config
                .extra
                .get("project_path")
                .cloned()
                .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
            Ok(byte_stream_to_chat_events_with_mode(
                byte_stream,
                start_time,
                span,
                config.model.clone(),
                project_path,
                config.langfuse_capture_output,
                config.langfuse_capture_max_chars,
                config.langfuse_redact_secrets,
                parse_usage_mode(config),
            ))
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            span.in_scope(|| {
                tracing::error!(target: "gen_ai.client.error", error_type = "rate_limit", "rate limit exceeded");
            });
            let retry_after = parse_retry_after(response.headers()).unwrap_or(60);
            Err(LlmError::RateLimit {
                retry_after_secs: retry_after,
            })
        } else if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            let body_text = response.text().await.unwrap_or_default();
            Err(LlmError::Auth(body_text))
        } else {
            span.in_scope(|| {
                tracing::error!(target: "gen_ai.client.error", error_type = "api_error", status = status.as_u16(), degraded, "API error");
            });
            let body_text = response.text().await.unwrap_or_default();
            Err(LlmError::Api {
                status: status.as_u16(),
                message: body_text,
            })
        }
    }
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
