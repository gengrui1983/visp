use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};
use std::pin::Pin;
use tracing::Instrument;
use tracing::field;
use visp_config::LlmConfig;
use visp_core::error::LlmError;
use visp_core::message::{Message, Role, ToolDefinition};
use visp_core::provider::{ChatEvent, LlmProvider};

use crate::image_util;
use crate::util::{build_client, parse_retry_after};

/// 构建 Anthropic API 请求体
pub fn build_anthropic_request(
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
) -> serde_json::Value {
    // 1. 提取 system 文本
    let system_text: String = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    // 2. 转换非 system 消息
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != Role::System).collect();
    let anthropic_messages = build_anthropic_messages(&non_system);

    // 3. 转换 tool definitions
    let anthropic_tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();

    // 4. 解析 thinking 配置并做合规处理（temperature=1.0，budget<max_tokens）
    let thinking_budget = match config.extra.get("thinking_budget_tokens") {
        Some(s) => match s.parse::<u32>() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("invalid thinking_budget_tokens value {:?}: {e}", s);
                None
            }
        },
        None => None,
    };
    let (temperature, thinking_field) = match thinking_budget {
        Some(budget) if config.max_tokens > 0 => {
            let clamped = if budget >= config.max_tokens {
                tracing::warn!(
                    budget,
                    max_tokens = config.max_tokens,
                    "thinking_budget_tokens >= max_tokens, clamping to max_tokens - 1"
                );
                config.max_tokens - 1
            } else {
                budget
            };
            (1.0_f64, Some(clamped))
        }
        Some(_) => {
            tracing::warn!("thinking_budget_tokens set but max_tokens=0, skipping thinking");
            (config.temperature, None)
        }
        None => (config.temperature, None),
    };

    // 5. 构建
    let mut request = serde_json::json!({
        "model": config.model,
        "messages": anthropic_messages,
        "max_tokens": config.max_tokens,
        "temperature": temperature,
    });

    if !system_text.is_empty() {
        // 使用数组格式 + cache_control 启用 prompt caching
        request["system"] = serde_json::json!([{
            "type": "text",
            "text": system_text,
            "cache_control": { "type": "ephemeral" }
        }]);
    }

    if config.use_tool && !anthropic_tools.is_empty() {
        // 给 tools 定义也加上 cache_control，缓存工具描述
        let mut tools_with_cache = anthropic_tools;
        if let Some(last) = tools_with_cache.last_mut()
            && let Some(obj) = last.as_object_mut()
        {
            obj.insert(
                "cache_control".to_string(),
                serde_json::json!({ "type": "ephemeral" }),
            );
        }
        request["tools"] = serde_json::Value::Array(tools_with_cache);
    }

    if let Some(budget) = thinking_field {
        request["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    // Anthropic API 流式请求
    request["stream"] = serde_json::Value::Bool(true);

    request
}

/// 构建请求头: Content-Type, x-api-key, anthropic-version, anthropic-beta (prompt caching)
pub fn build_anthropic_headers(api_key: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    headers.insert("x-api-key", api_key.parse().unwrap());
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.insert(
        "anthropic-beta",
        "prompt-caching-2024-07-31".parse().unwrap(),
    );
    headers.insert(reqwest::header::USER_AGENT, "visp/0.1.0".parse().unwrap());
    headers
}

/// 将 visp-core 消息转换为 Anthropic Messages API 格式
///
/// 规则：
/// - tool role 合并到最近一条 user 消息的 tool_result content block
/// - 连续同角色消息合并（文本用 \n\n 拼接）
/// - 消息按时间顺序排列，包含 tool_result 的 user 消息会移动到末尾
fn build_anthropic_messages(messages: &[&Message]) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        if msg.role == Role::Tool {
            let tool_use_id = msg.tool_call_id.as_deref().unwrap_or_else(|| {
                tracing::error!("Tool message without tool_call_id");
                ""
            });
            let tool_result = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": msg.content,
            });
            // 如果上一条消息已经是 user(tool_result)，追加到同一消息
            // (Anthropic 要求所有 tool_result 在同一 user 消息中)
            if let Some(last) = result.last_mut()
                && last["role"] == "user"
                && last["content"]
                    .as_array()
                    .is_some_and(|a| a.iter().all(|b| b["type"] == "tool_result"))
            {
                last["content"].as_array_mut().unwrap().push(tool_result);
            } else {
                result.push(serde_json::json!({
                    "role": "user",
                    "content": [tool_result],
                }));
            }
            continue;
        }

        // User / Assistant 消息（System 和 Tool 已在上面过滤掉）
        let role_str = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            _ => panic!("unexpected role in anthropic message: {:?}", msg.role),
        };

        // 构建 content blocks：extra_blocks + text + tool_use
        let mut content_blocks: Vec<serde_json::Value> = Vec::new();

        // 来自 API 的额外内容块（如 thinking），原样保留
        if let Some(ref blocks) = msg.extra_blocks {
            content_blocks.extend(blocks.iter().cloned());
        }

        // text block
        if !msg.content.is_empty() {
            content_blocks.push(serde_json::json!({
                "type": "text",
                "text": msg.content,
            }));
        }

        // image blocks (multimodal vision)
        for img in &msg.images {
            content_blocks.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.mime_type,
                    "data": img.base64,
                }
            }));
        }

        // tool_use blocks（仅 assistant 消息有 tool_calls）
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                let input: serde_json::Value = match serde_json::from_str(&call.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "failed to parse tool call arguments for '{}': {e}",
                            call.name
                        );
                        serde_json::Value::Object(serde_json::Map::new())
                    }
                };
                content_blocks.push(serde_json::json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
        }

        // 检查是否与上一条同角色消息合并
        if let Some(last) = result.last_mut()
            && last["role"] == role_str
            && !content_blocks.is_empty()
        {
            // 合并：将新 text 拼接到上一条的最后一个 text block
            let existing = last["content"].as_array_mut().unwrap();
            for block in content_blocks {
                if block["type"] == "text"
                    && existing
                        .last()
                        .map(|b| b["type"] == "text")
                        .unwrap_or(false)
                {
                    let last_text = existing.last_mut().unwrap();
                    let new_text = format!(
                        "{}\n\n{}",
                        last_text["text"].as_str().unwrap_or(""),
                        block["text"].as_str().unwrap_or("")
                    );
                    last_text["text"] = serde_json::Value::String(new_text);
                } else {
                    existing.push(block);
                }
            }
            continue;
        }

        // 创建新消息
        result.push(serde_json::json!({
            "role": role_str,
            "content": content_blocks,
        }));
    }

    result
}

/// SSE 解析中间结果
#[derive(Debug)]
pub(crate) enum ParsedEvent {
    Emit(ChatEvent),
    /// 工具输入增量: (index, tool_id, tool_name, partial_input_json)
    #[allow(dead_code)]
    ToolInputDelta {
        index: u64,
        id: String,
        name: String,
        partial: String,
    },
    /// 内容块结束（工具或思考）: index
    BlockStop {
        index: u64,
    },
    /// 思考增量: (index, partial_text, signature)
    ThinkingDelta {
        index: u64,
        partial: String,
        signature: String,
    },
    /// 图片内容块: (base64 数据, MIME 类型, 远程 URL)
    ImageBlock {
        data: Option<String>,
        mime_type: String,
        remote_url: Option<String>,
    },
    /// token 用量信息及消息元数据
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// 模型名称（仅 message_start 有值）
        model: Option<String>,
        /// 结束原因（仅 message_delta 有值；message_start 中通常为 null）
        stop_reason: Option<String>,
    },
    Skip,
}

/// Anthropic SSE 事件解析（带工具输入增量累积）
///
/// 将 SSE 事件名和 JSON data 映射为 ParsedEvent。
/// - `content_block_delta` + `text_delta` → `Emit(ChatEvent::TextDelta)`
/// - `content_block_delta` + `input_json_delta` → `ToolInputDelta { index, .. }`
/// - `content_block_start` + `tool_use` → `ToolInputDelta { id, name, partial: initial_input }`
/// - `content_block_stop` → `ToolBlockStop { index }`
/// - `message_stop` → `Emit(ChatEvent::Done)`
/// - 其他事件 → `Skip`
pub(crate) fn parse_anthropic_event(event_name: &str, data: &str) -> Result<ParsedEvent, LlmError> {
    match event_name {
        "message_start" => {
            let v: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Stream(format!("parse message_start: {e}")))?;
            let input_tokens = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
            let output_tokens = v["message"]["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
            let cache_creation = v["message"]["usage"]["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
            let cache_read = v["message"]["usage"]["cache_read_input_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
            let model = v["message"]["model"].as_str().map(|s| s.to_string());
            let stop_reason = v["message"]["stop_reason"].as_str().map(|s| s.to_string());
            Ok(ParsedEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
                model,
                stop_reason,
            })
        }
        "message_delta" => {
            let v: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Stream(format!("parse message_delta: {e}")))?;
            let output_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
            let stop_reason = v["delta"]["stop_reason"].as_str().map(|s| s.to_string());
            Ok(ParsedEvent::Usage {
                input_tokens: 0,
                output_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                model: None,
                stop_reason,
            })
        }
        "message_stop" => Ok(ParsedEvent::Emit(ChatEvent::Done)),
        "content_block_delta" => {
            let v: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Stream(format!("parse content_block_delta: {e}")))?;
            let delta_type = v["delta"]["type"].as_str().unwrap_or_else(|| {
                tracing::warn!("anthropic SSE content_block_delta without type");
                ""
            });
            let index = v["index"].as_u64().unwrap_or(0);
            match delta_type {
                "text_delta" => {
                    let text = v["delta"]["text"].as_str().unwrap_or("").to_string();
                    Ok(ParsedEvent::Emit(ChatEvent::TextDelta(text)))
                }
                "input_json_delta" => {
                    let partial = v["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    Ok(ParsedEvent::ToolInputDelta {
                        index,
                        id: String::new(),
                        name: String::new(),
                        partial,
                    })
                }
                "thinking_delta" => {
                    let partial = v["delta"]["thinking"].as_str().unwrap_or("").to_string();
                    Ok(ParsedEvent::ThinkingDelta {
                        index,
                        partial,
                        signature: String::new(),
                    })
                }
                _ => Ok(ParsedEvent::Skip),
            }
        }
        "content_block_start" => {
            let v: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Stream(format!("parse content_block_start: {e}")))?;
            let index = v["index"].as_u64().unwrap_or(0);
            let block_type = v["content_block"]["type"].as_str().unwrap_or_else(|| {
                tracing::warn!("anthropic SSE content_block_start without type");
                ""
            });
            match block_type {
                "tool_use" => {
                    let id = v["content_block"]["id"]
                        .as_str()
                        .unwrap_or_else(|| {
                            tracing::warn!("anthropic SSE tool_use block without id");
                            ""
                        })
                        .to_string();
                    let name = v["content_block"]["name"]
                        .as_str()
                        .unwrap_or_else(|| {
                            tracing::warn!("anthropic SSE tool_use block without name");
                            ""
                        })
                        .to_string();
                    // 流式下 content_block_start 的 input 通常为 {}，真正的参数通过 input_json_delta 发送。
                    // 但有时（如大内容场景）API 可能直接在 content_block_start 中返回完整参数。
                    // 检查初始 input，非空时保存为起始 partial JSON，否则从空字符串开始累积。
                    let partial = match v["content_block"]["input"].as_object() {
                        Some(obj) if obj.is_empty() => String::new(),
                        Some(obj) => serde_json::to_string(obj).unwrap_or_default(),
                        None => String::new(),
                    };
                    Ok(ParsedEvent::ToolInputDelta {
                        index,
                        id,
                        name,
                        partial,
                    })
                }
                "thinking" => {
                    let signature = v["content_block"]["signature"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let thinking = v["content_block"]["thinking"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    Ok(ParsedEvent::ThinkingDelta {
                        index,
                        partial: thinking,
                        signature,
                    })
                }
                "redacted_thinking" => {
                    let signature = v["content_block"]["signature"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    Ok(ParsedEvent::ThinkingDelta {
                        index,
                        partial: "[REDACTED]".to_string(),
                        signature,
                    })
                }
                "image" => {
                    let source = &v["content_block"]["source"];
                    let source_type = source["type"].as_str().unwrap_or("");
                    match source_type {
                        "base64" => {
                            let media_type = source["media_type"]
                                .as_str()
                                .unwrap_or("image/png")
                                .to_string();
                            let data = source["data"].as_str().unwrap_or("").to_string();
                            Ok(ParsedEvent::ImageBlock {
                                data: Some(data),
                                mime_type: media_type,
                                remote_url: None,
                            })
                        }
                        "url" => {
                            let url = source["url"].as_str().unwrap_or("").to_string();
                            Ok(ParsedEvent::ImageBlock {
                                data: None,
                                mime_type: String::new(),
                                remote_url: Some(url),
                            })
                        }
                        _ => Ok(ParsedEvent::Skip),
                    }
                }
                _ => Ok(ParsedEvent::Skip),
            }
        }
        "content_block_stop" => {
            let v: serde_json::Value = serde_json::from_str(data)
                .map_err(|e| LlmError::Stream(format!("parse content_block_stop: {e}")))?;
            let index = v["index"].as_u64().unwrap_or(0);
            // 同时触发 thinking 和 tool_use 的 flush（由 byte_stream 按 index 区分）
            Ok(ParsedEvent::BlockStop { index })
        }
        _ => Ok(ParsedEvent::Skip),
    }
}

/// 检查 base_url 是否已经包含版本路径段（如 /v1, /v3 等）。
fn is_versioned_base_url(url: &str) -> bool {
    url.rsplit('/')
        .next()
        .is_some_and(|seg| seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit()))
}

/// Anthropic API 提供器
pub struct AnthropicProvider {
    api_key: String,
    api_url: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            api_url: "https://api.anthropic.com".to_string(),
            client: build_client(),
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            api_url: base_url,
            client: build_client(),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        config: &LlmConfig,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        // Provider name from config (falls back to "anthropic" for backward compat)
        let provider_name = config.provider.as_deref().unwrap_or("anthropic");
        // 创建 gen_ai.client.operation span（OTel Semantic Conventions 标准命名）
        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.system = field::Empty,
            gen_ai.request.model = %config.model,
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = %provider_name,
            gen_ai.request.max_tokens = field::Empty,
            gen_ai.request.temperature = field::Empty,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = field::Empty,
            gen_ai.usage.output_tokens = field::Empty,
            gen_ai.usage.cache_read.input_tokens = field::Empty,
            gen_ai.usage.cache_creation.input_tokens = field::Empty,
            gen_ai.response.finish_reasons = field::Empty,
            gen_ai.response.model = field::Empty,
            visp.llm.token_limit_hit = field::Empty,
            langfuse.observation.type = field::Empty,
            langfuse.observation.input = field::Empty,
            visp.tools.definitions = field::Empty,
            langfuse.observation.output = field::Empty,
            langfuse.session.id = field::Empty,
            langfuse.trace.name = field::Empty,
            langfuse.user.id = field::Empty,
            langfuse.trace.tags = field::Empty,
            langfuse.environment = field::Empty,
            langfuse.release = field::Empty,
            langfuse.version = field::Empty,
            langfuse.trace.public = field::Empty,
            langfuse.trace.metadata = field::Empty,
            gen_ai.client.base_url = field::Empty,
        );
        span.record("gen_ai.system", "anthropic");
        span.record("gen_ai.request.max_tokens", config.max_tokens as i64);
        span.record("gen_ai.request.temperature", config.temperature);
        span.record("gen_ai.client.base_url", self.api_url.as_str());

        // Langfuse trace-level fields: record when enabled
        if config.langfuse_enabled {
            if let Some(ref val) = config.langfuse_session_id {
                span.record("langfuse.session.id", val.as_str());
            }
            if let Some(ref val) = config.langfuse_trace_name {
                span.record("langfuse.trace.name", val.as_str());
            }
            if let Some(ref val) = config.langfuse_user_id {
                span.record("langfuse.user.id", val.as_str());
            }
            if let Some(ref val) = config.langfuse_tags {
                span.record("langfuse.trace.tags", val.as_str());
            }
            if let Some(ref val) = config.langfuse_environment {
                span.record("langfuse.environment", val.as_str());
            }
            if let Some(ref val) = config.langfuse_release {
                span.record("langfuse.release", val.as_str());
            }
            if let Some(ref val) = config.langfuse_version {
                span.record("langfuse.version", val.as_str());
            }
            if let Some(public) = config.langfuse_public {
                span.record("langfuse.trace.public", public);
            }
            if let Some(ref metadata) = config.langfuse_metadata
                && !metadata.is_empty()
                && let Ok(json) = serde_json::to_string(metadata)
            {
                span.record("langfuse.trace.metadata", json.as_str());
            }
        }

        let base = self.api_url.trim_end_matches('/');
        let url = if is_versioned_base_url(base) {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        let body = build_anthropic_request(messages, tools, config);
        let headers = build_anthropic_headers(&self.api_key);

        // Langfuse generation capture: record input if enabled
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

        // 诊断统计：历史回传中的可疑块（空 input 的 tool_use / 无 signature 的
        // thinking）。第三方 Anthropic 兼容端点对这类畸形历史可能返回空响应。
        let mut empty_input_tool_uses = 0usize;
        let mut thinking_blocks = 0usize;
        let mut unsigned_thinking_blocks = 0usize;
        if let Some(msgs) = body["messages"].as_array() {
            for m in msgs {
                if let Some(blocks) = m["content"].as_array() {
                    for b in blocks {
                        match b["type"].as_str().unwrap_or("") {
                            "tool_use" => {
                                if b["input"].as_object().is_some_and(|o| o.is_empty()) {
                                    empty_input_tool_uses += 1;
                                }
                            }
                            "thinking" | "redacted_thinking" => {
                                thinking_blocks += 1;
                                if b["signature"].as_str().unwrap_or("").is_empty() {
                                    unsigned_thinking_blocks += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        tracing::debug!(
            url = %url,
            model = %config.model,
            messages = body["messages"].as_array().map(|a| a.len()).unwrap_or(0),
            tools = body["tools"].as_array().map(|a| a.len()).unwrap_or(0),
            max_tokens = body["max_tokens"].as_u64().unwrap_or(0),
            thinking_budget = body["thinking"]["budget_tokens"].as_u64(),
            empty_input_tool_uses,
            thinking_blocks,
            unsigned_thinking_blocks,
            "Anthropic request"
        );
        tracing::trace!(body = %serde_json::to_string(&body).unwrap_or_default(), "Anthropic request body");
        let start_time = std::time::Instant::now();
        let send_fut = self.client.post(&url).headers(headers).json(&body).send();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            resp = send_fut => resp.map_err(|e| LlmError::Network(e.to_string()))?,
        };

        let status = response.status();
        if status.is_success() {
            let byte_stream = response.bytes_stream();
            let project_path = config
                .extra
                .get("project_path")
                .cloned()
                .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
            Ok(byte_stream_to_chat_events(
                byte_stream,
                start_time,
                span,
                config.model.clone(),
                project_path,
                config.langfuse_capture_output,
                config.langfuse_capture_max_chars,
                config.langfuse_redact_secrets,
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
                tracing::error!(target: "gen_ai.client.error", error_type = "api_error", status = status.as_u16(), "API error");
            });
            let body_text = response.text().await.unwrap_or_default();
            Err(LlmError::Api {
                status: status.as_u16(),
                message: body_text,
            })
        }
    }
}

/// 归一化 Anthropic stop_reason 为 OTel 标准 finish_reason
///
/// - `max_tokens` → `length`
/// - 其余值原样保留
fn normalize_anthropic_reason(reason: &str) -> &str {
    match reason {
        "max_tokens" => "length",
        other => other,
    }
}

/// 将 `reqwest` 的字节流转换为 `ChatEvent` 流
///
/// 累积字节直到遇到 `\n\n` 分隔符，然后用 `parse_sse_events` 解析
/// 每个完整的 SSE 事件，最后用 `parse_anthropic_event` 映射为 ChatEvent。
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
    use std::collections::HashMap;

    struct ToolAcc {
        id: String,
        name: String,
        input: String,
    }

    struct ThinkingAcc {
        signature: String,
        thinking: String,
    }

    struct StreamState {
        stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
        buf: String,
        /// 跨 chunk 的不完整 UTF-8 尾字节缓冲（修复多字节字符被 TCP chunk 切坏的问题）
        pending_bytes: Vec<u8>,
        tools: HashMap<String, ToolAcc>,
        thinking_acc: HashMap<String, ThinkingAcc>,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// 已上报给 UsageDelta 的累计值（与最终统计字段区分，仅供实时速率展示）
        reported_input_tokens: u32,
        reported_output_tokens: u32,
        reported_cache_creation_input_tokens: u32,
        reported_cache_read_input_tokens: u32,
        /// 响应模型名称（从 message_start 提取）
        model: String,
        /// 请求时的模型名称（用于 response model 为空时的 fallback）
        request_model: String,
        /// 项目根路径（用于保存 base64 图片）
        project_path: String,
        /// 响应结束原因（从 message_delta 提取）
        stop_reason: String,
        /// 请求开始时刻（用于计算端到端 latency）
        start_time: std::time::Instant,
        /// 设为 true 后，下次 unfold 迭代发射 UsageInfo，再下次发射 OutputMetadata，再下次发射 Done
        done_pending: bool,
        /// UsageInfo 已发射
        usage_emitted: bool,
        /// OutputMetadata 已发射
        metadata_emitted: bool,
        /// gen_ai.client.operation span（用于 record 完成时字段）
        span: tracing::Span,
        /// 是否已发射首 token 事件
        first_content_emitted: bool,
        /// Langfuse 输出捕获配置
        langfuse_capture_output: bool,
        langfuse_capture_max_chars: usize,
        langfuse_redact_secrets: bool,
        /// 累积的输出文本（用于 langfuse.observation.output）
        accumulated_output: String,
    }

    let state = StreamState {
        stream: Box::pin(byte_stream),
        buf: String::new(),
        pending_bytes: Vec::new(),
        tools: HashMap::new(),
        thinking_acc: HashMap::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        reported_input_tokens: 0,
        reported_output_tokens: 0,
        reported_cache_creation_input_tokens: 0,
        reported_cache_read_input_tokens: 0,
        model: String::new(),
        request_model,
        project_path,
        stop_reason: String::new(),
        start_time,
        done_pending: false,
        usage_emitted: false,
        metadata_emitted: false,
        span,
        first_content_emitted: false,
        langfuse_capture_output,
        langfuse_capture_max_chars,
        langfuse_redact_secrets,
        accumulated_output: String::new(),
    };

    /// 标记首 token 已到达（幂等，仅首次有效）
    fn emit_first_token(state: &mut StreamState) {
        if !state.first_content_emitted {
            state.first_content_emitted = true;
            tracing::debug!(target: "gen_ai.client.first_token", "first token received");
        }
    }

    let event_stream = stream::unfold(state, |mut state| {
        let span = state.span.clone();
        async move {
            // 第 1 阶段：所有 SSE 处理完毕，发射 UsageInfo
            if state.done_pending && !state.usage_emitted {
                state.usage_emitted = true;
                return Some((
                    Ok(ChatEvent::UsageInfo {
                        input_tokens: state.input_tokens,
                        output_tokens: state.output_tokens,
                        tool_calls: 0,
                        cache_creation_input_tokens: state.cache_creation_input_tokens,
                        cache_read_input_tokens: state.cache_read_input_tokens,
                        cost: None,
                    }),
                    state,
                ));
            }
            // 第 2 阶段：发射 OutputMetadata（包含 ProviderMetadata）
            if state.usage_emitted && !state.metadata_emitted {
                state.metadata_emitted = true;
                let latency = state.start_time.elapsed().as_millis() as u64;
                let normalized_stop_reason = normalize_anthropic_reason(&state.stop_reason);
                let finish_reasons = if state.stop_reason.is_empty() {
                    vec![]
                } else {
                    vec![normalized_stop_reason.to_string()]
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
                if state.cache_read_input_tokens > 0 {
                    state.span.record(
                        "gen_ai.usage.cache_read.input_tokens",
                        state.cache_read_input_tokens as i64,
                    );
                }
                if state.cache_creation_input_tokens > 0 {
                    state.span.record(
                        "gen_ai.usage.cache_creation.input_tokens",
                        state.cache_creation_input_tokens as i64,
                    );
                }
                if state.stop_reason == "max_tokens" {
                    state.span.record("visp.llm.token_limit_hit", true);
                }
                let finish_reasons_str = serde_json::to_string(&finish_reasons).unwrap_or_default();
                state
                    .span
                    .record("gen_ai.response.finish_reasons", &finish_reasons_str);
                // Fallback to request model if response didn't include one
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
                        cache_creation_input_tokens: if state.cache_creation_input_tokens > 0 {
                            Some(state.cache_creation_input_tokens)
                        } else {
                            None
                        },
                        latency_ms: latency,
                    })),
                    state,
                ));
            }
            // 第 3 阶段：发射 Done + completed event
            if state.metadata_emitted {
                state.done_pending = false;
                state.usage_emitted = false;
                state.metadata_emitted = false;
                tracing::debug!(
                    target: "gen_ai.client.completed",
                    input_tokens = state.input_tokens,
                    output_tokens = state.output_tokens,
                    model = %state.model,
                    "LLM request completed"
                );
                return Some((Ok(ChatEvent::Done), state));
            }

            /// 更新 token 计数及消息元数据
            fn update_anthropic_usage(
                state: &mut StreamState,
                input_tokens: u32,
                output_tokens: u32,
                cache_creation_input_tokens: u32,
                cache_read_input_tokens: u32,
                model: Option<String>,
                stop_reason: Option<String>,
            ) {
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
                if let Some(m) = model
                    && !m.is_empty()
                {
                    state.model = m;
                }
                if let Some(sr) = stop_reason
                    && !sr.is_empty()
                {
                    state.stop_reason = sr;
                }
            }

            /// 计算本次 usage 相对「已上报累计值」的增量，并更新已上报值。
            ///
            /// 返回的 `UsageDelta` 仅供 CLI 实时速率展示；最终结算仍以流末
            /// `UsageInfo` 为准，因此这里只更新 `reported_*` 字段，不影响
            /// `input_tokens` 等最终统计字段。
            fn report_usage_delta(
                state: &mut StreamState,
                input_tokens: u32,
                output_tokens: u32,
                cache_creation_input_tokens: u32,
                cache_read_input_tokens: u32,
            ) -> Option<ChatEvent> {
                // 相对已上报值求差分（饱和减法防止累计值回退时产生下溢）
                let delta_in = input_tokens.saturating_sub(state.reported_input_tokens);
                let delta_out = output_tokens.saturating_sub(state.reported_output_tokens);
                let delta_cc = cache_creation_input_tokens
                    .saturating_sub(state.reported_cache_creation_input_tokens);
                let delta_cr =
                    cache_read_input_tokens.saturating_sub(state.reported_cache_read_input_tokens);
                // 已上报值只增不减（message_delta 只携带 output_tokens，其余字段为 0，
                // 不应把已上报的 input/cache 值回退）
                state.reported_input_tokens = state.reported_input_tokens.max(input_tokens);
                state.reported_output_tokens = state.reported_output_tokens.max(output_tokens);
                state.reported_cache_creation_input_tokens = state
                    .reported_cache_creation_input_tokens
                    .max(cache_creation_input_tokens);
                state.reported_cache_read_input_tokens = state
                    .reported_cache_read_input_tokens
                    .max(cache_read_input_tokens);

                // 无增量时不发射，保持事件流不变
                if delta_in > 0 || delta_out > 0 || delta_cc > 0 || delta_cr > 0 {
                    Some(ChatEvent::UsageDelta {
                        input_tokens: delta_in,
                        output_tokens: delta_out,
                        cache_creation_input_tokens: delta_cc,
                        cache_read_input_tokens: delta_cr,
                    })
                } else {
                    None
                }
            }

            /// 构建 UsageInfo（表示 Done 事件）
            fn build_done_usage_info(state: &StreamState) -> ChatEvent {
                ChatEvent::UsageInfo {
                    input_tokens: state.input_tokens,
                    output_tokens: state.output_tokens,
                    tool_calls: 0,
                    cache_creation_input_tokens: state.cache_creation_input_tokens,
                    cache_read_input_tokens: state.cache_read_input_tokens,
                    cost: None,
                }
            }

            loop {
                if let Some(pos) = state.buf.find("\n\n") {
                    let chunk = state.buf[..pos].to_string();
                    state.buf = state.buf[pos + 2..].to_string();

                    let sse_events = crate::streaming::parse_sse_events(&chunk);
                    for sse in sse_events {
                        let event_name = sse.event.as_deref().unwrap_or("");
                        let data = sse.data.as_deref().unwrap_or("");
                        tracing::trace!(
                            target: "visp.sse.anthropic",
                            event = event_name,
                            data,
                            "raw SSE event"
                        );
                        match parse_anthropic_event(event_name, data) {
                            Ok(ParsedEvent::Emit(chat_event)) => {
                                // message_stop → 先标记 pending，返回后发 UsageInfo
                                if matches!(chat_event, ChatEvent::Done) {
                                    state.done_pending = true;
                                    state.usage_emitted = true;
                                    return Some((Ok(build_done_usage_info(&state)), state));
                                }
                                // Accumulate output text for Langfuse capture
                                if state.langfuse_capture_output
                                    && let ChatEvent::TextDelta(ref text) = chat_event
                                {
                                    state.accumulated_output.push_str(text);
                                }
                                emit_first_token(&mut state);
                                return Some((Ok(chat_event), state));
                            }
                            Ok(ParsedEvent::Usage {
                                input_tokens,
                                output_tokens,
                                cache_creation_input_tokens,
                                cache_read_input_tokens,
                                model,
                                stop_reason,
                            }) => {
                                update_anthropic_usage(
                                    &mut state,
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    model,
                                    stop_reason,
                                );
                                // 逐事件上报增量（供 CLI 实时速率显示；最终总量仍以
                                // 流末 UsageInfo 为准，因此调用方不应把它累加进结算统计）
                                if let Some(evt) = report_usage_delta(
                                    &mut state,
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                ) {
                                    return Some((Ok(evt), state));
                                }
                            }
                            Ok(ParsedEvent::ThinkingDelta {
                                index,
                                partial,
                                signature,
                            }) => {
                                let key = format!("thinking_{}", index);
                                let entry =
                                    state
                                        .thinking_acc
                                        .entry(key)
                                        .or_insert_with(|| ThinkingAcc {
                                            signature: String::new(),
                                            thinking: String::new(),
                                        });
                                if !signature.is_empty() {
                                    entry.signature = signature;
                                }
                                entry.thinking.push_str(&partial);
                                // 流式发射 partial ThinkingBlock
                                let block = serde_json::json!({
                                    "type": "thinking",
                                    "thinking": entry.thinking.clone(),
                                    "signature": entry.signature.clone(),
                                });
                                emit_first_token(&mut state);
                                return Some((Ok(ChatEvent::ThinkingBlock(block)), state));
                            }
                            Ok(ParsedEvent::ToolInputDelta {
                                index,
                                id,
                                name,
                                partial,
                            }) => {
                                let key = index.to_string();
                                if !name.is_empty() {
                                    state.tools.insert(
                                        key,
                                        ToolAcc {
                                            id,
                                            name,
                                            input: partial,
                                        },
                                    );
                                } else if let Some(acc) = state.tools.get_mut(&key) {
                                    acc.input.push_str(&partial);
                                }
                            }
                            Ok(ParsedEvent::BlockStop { index }) => {
                                let tkey = index.to_string();
                                if let Some(acc) = state.thinking_acc.remove(&tkey) {
                                    let block = serde_json::json!({
                                        "type": "thinking",
                                        "thinking": acc.thinking,
                                        "signature": acc.signature,
                                    });
                                    emit_first_token(&mut state);
                                    return Some((Ok(ChatEvent::ThinkingBlock(block)), state));
                                }
                                let key = index.to_string();
                                if let Some(acc) = state.tools.remove(&key) {
                                    let evt = ChatEvent::ToolCall {
                                        id: acc.id,
                                        name: acc.name,
                                        arguments: acc.input,
                                    };
                                    emit_first_token(&mut state);
                                    return Some((Ok(evt), state));
                                }
                            }
                            Ok(ParsedEvent::ImageBlock {
                                data,
                                mime_type,
                                remote_url,
                            }) => {
                                emit_first_token(&mut state);
                                if let Some(base64_data) = data {
                                    // base64 source: save to disk
                                    match image_util::save_base64_image(
                                        &base64_data,
                                        &mime_type,
                                        &state.project_path,
                                    ) {
                                        Ok(path) => {
                                            return Some((
                                                Ok(ChatEvent::ImageBlock {
                                                    path,
                                                    mime_type,
                                                    remote_url: None,
                                                }),
                                                state,
                                            ));
                                        }
                                        Err(e) => {
                                            return Some((
                                                Ok(ChatEvent::ImageError {
                                                    reason: e.to_string(),
                                                }),
                                                state,
                                            ));
                                        }
                                    }
                                } else if let Some(url) = remote_url {
                                    // URL source: pass through, CLI will lazy-load
                                    return Some((
                                        Ok(ChatEvent::ImageBlock {
                                            path: String::new(),
                                            mime_type: String::new(),
                                            remote_url: Some(url),
                                        }),
                                        state,
                                    ));
                                } else {
                                    // Neither data nor URL - skip
                                    continue;
                                }
                            }
                            Ok(ParsedEvent::Skip) => continue,
                            Err(e) => {
                                return Some((Err(e), state));
                            }
                        }
                    }
                    continue;
                }

                match state.stream.next().await {
                    Some(Ok(bytes)) => {
                        state.pending_bytes.extend_from_slice(&bytes);
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
                        return Some((Err(LlmError::Stream(e.to_string())), state));
                    }
                    None => {
                        if !state.buf.is_empty() {
                            let sse_events = crate::streaming::parse_sse_events(&state.buf);
                            for sse in sse_events {
                                let event_name = sse.event.as_deref().unwrap_or("");
                                let data = sse.data.as_deref().unwrap_or("");
                                match parse_anthropic_event(event_name, data) {
                                    Ok(ParsedEvent::Emit(chat_event)) => {
                                        if matches!(chat_event, ChatEvent::Done) {
                                            state.done_pending = true;
                                            state.usage_emitted = true;
                                            return Some((
                                                Ok(build_done_usage_info(&state)),
                                                state,
                                            ));
                                        }
                                        // Accumulate output text for Langfuse capture
                                        if state.langfuse_capture_output
                                            && let ChatEvent::TextDelta(ref text) = chat_event
                                        {
                                            state.accumulated_output.push_str(text);
                                        }
                                        emit_first_token(&mut state);
                                        return Some((Ok(chat_event), state));
                                    }
                                    Ok(ParsedEvent::Usage {
                                        input_tokens,
                                        output_tokens,
                                        cache_creation_input_tokens,
                                        cache_read_input_tokens,
                                        model,
                                        stop_reason,
                                    }) => {
                                        update_anthropic_usage(
                                            &mut state,
                                            input_tokens,
                                            output_tokens,
                                            cache_creation_input_tokens,
                                            cache_read_input_tokens,
                                            model,
                                            stop_reason,
                                        );
                                        // 逐事件上报增量（与主循环一致；无增量时不发射）
                                        if let Some(evt) = report_usage_delta(
                                            &mut state,
                                            input_tokens,
                                            output_tokens,
                                            cache_creation_input_tokens,
                                            cache_read_input_tokens,
                                        ) {
                                            return Some((Ok(evt), state));
                                        }
                                        continue;
                                    }
                                    Ok(_) => continue,
                                    Err(e) => {
                                        return Some((Err(e), state));
                                    }
                                }
                            }
                        }
                        return None;
                    }
                }
            }
        }
        .instrument(span)
    });

    Box::pin(event_stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- AnthropicProvider 构造测试 ---

    #[test]
    fn test_provider_with_base_url() {
        let provider =
            AnthropicProvider::with_base_url("test-key".into(), "https://custom.api.com".into());
        assert_eq!(provider.api_url, "https://custom.api.com");
    }

    #[test]
    fn test_provider_default_url() {
        let provider = AnthropicProvider::new("test-key".into());
        assert_eq!(provider.api_url, "https://api.anthropic.com");
    }

    // --- parse_anthropic_event 测试 ---

    #[test]
    fn test_parse_text_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_anthropic_event("content_block_delta", data).unwrap();
        match result {
            ParsedEvent::Emit(ChatEvent::TextDelta(t)) => assert_eq!(t, "Hello"),
            _ => panic!("expected TextDelta, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_tool_use_start_with_empty_input() {
        // 流式模式：input 为 {}，后续由 input_json_delta 填充
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_123","name":"get_weather","input":{}}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        match result {
            ParsedEvent::ToolInputDelta {
                index,
                id,
                name,
                partial,
            } => {
                assert_eq!(id, "toolu_123");
                assert_eq!(name, "get_weather");
                assert_eq!(index, 0);
                assert!(
                    partial.is_empty(),
                    "empty input should result in empty partial"
                );
            }
            _ => panic!("expected ToolInputDelta, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_tool_use_start_with_initial_input() {
        // 非流式/大内容场景：input 直接包含完整参数
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_456","name":"write_file","input":{"path":"test.txt","content":"hello"}}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        match result {
            ParsedEvent::ToolInputDelta {
                index,
                id,
                name,
                partial,
            } => {
                assert_eq!(id, "toolu_456");
                assert_eq!(name, "write_file");
                assert_eq!(index, 0);
                assert!(
                    !partial.is_empty(),
                    "non-empty input should be preserved as partial"
                );
                // partial 应为 JSON 字符串
                let parsed: serde_json::Value = serde_json::from_str(&partial).unwrap();
                assert_eq!(parsed["path"], "test.txt");
                assert_eq!(parsed["content"], "hello");
            }
            _ => panic!("expected ToolInputDelta, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_message_stop() {
        let result = parse_anthropic_event("message_stop", r#"{"type":"message_stop"}"#).unwrap();
        match result {
            ParsedEvent::Emit(ChatEvent::Done) => {}
            _ => panic!("expected Done, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_message_start_returns_usage_and_model() {
        let data = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":52,"cache_read_input_tokens":200}}}"#;
        let result = parse_anthropic_event("message_start", data).unwrap();
        match result {
            ParsedEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                model,
                stop_reason,
            } => {
                assert_eq!(input_tokens, 105);
                assert_eq!(output_tokens, 0);
                assert_eq!(cache_creation_input_tokens, 52);
                assert_eq!(cache_read_input_tokens, 200);
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert_eq!(stop_reason, None);
            }
            _ => panic!("expected Usage, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_message_delta_returns_usage() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":125}}"#;
        let result = parse_anthropic_event("message_delta", data).unwrap();
        match result {
            ParsedEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(input_tokens, 0);
                assert_eq!(output_tokens, 125);
            }
            _ => panic!("expected Usage, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_unknown_event_returns_skip() {
        let result = parse_anthropic_event("ping", "{}").unwrap();
        assert!(matches!(result, ParsedEvent::Skip));
    }

    #[test]
    fn test_parse_text_block_start_returns_skip() {
        let data =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        assert!(matches!(result, ParsedEvent::Skip));
    }

    #[test]
    fn test_parse_redacted_thinking_block_start() {
        let data = r#"{"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","signature":"base64sig123"}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        match result {
            ParsedEvent::ThinkingDelta {
                index,
                partial,
                signature,
            } => {
                assert_eq!(index, 2);
                assert_eq!(partial, "[REDACTED]");
                assert_eq!(signature, "base64sig123");
            }
            _ => panic!("expected ThinkingDelta, got {:?}", result),
        }
    }

    // --- parse_retry_after 测试 ---

    #[test]
    fn test_parse_429_rate_limit() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("30"),
        );
        assert_eq!(parse_retry_after(&headers), Some(30));
    }

    #[test]
    fn test_parse_retry_after_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn test_parse_retry_after_invalid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("not-a-number"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn test_build_messages_basic() {
        let msgs = vec![Message::user("Hello"), Message::assistant("Hi there!")];
        let config = LlmConfig {
            model: "claude-sonnet-4-20250514".into(),
            temperature: 0.7,
            max_tokens: 4096,
            max_context_tokens: 128_000,
            extra: Default::default(),
            ..Default::default()
        };
        let req = build_anthropic_request(&msgs, &[], &config);
        assert_eq!(req["model"], "claude-sonnet-4-20250514");
        assert_eq!(req["max_tokens"], 4096);
        assert_eq!(req["temperature"], 0.7);
        assert!(req.get("system").is_none());

        let msgs_arr = req["messages"].as_array().unwrap();
        assert_eq!(msgs_arr.len(), 2);
        assert_eq!(msgs_arr[0]["role"], "user");
        assert_eq!(msgs_arr[0]["content"][0]["type"], "text");
        assert_eq!(msgs_arr[0]["content"][0]["text"], "Hello");
        assert_eq!(msgs_arr[1]["role"], "assistant");
        assert_eq!(msgs_arr[1]["content"][0]["text"], "Hi there!");
    }

    #[test]
    fn test_build_anthropic_messages_with_images() {
        use visp_core::message::ImageData;
        let mut msg = Message::user("这是什么？");
        msg.images = vec![ImageData {
            path: "/tmp/test.png".to_string(),
            base64: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        }];
        let messages = build_anthropic_messages(&[&msg]);
        let user_msg = &messages[0];
        assert_eq!(user_msg["role"], "user");
        let content = user_msg["content"].as_array().unwrap();
        // Should have text block + image block
        assert!(content.iter().any(|b| b["type"] == "text"));
        assert!(content.iter().any(|b| b["type"] == "image"));
        let img_block = content.iter().find(|b| b["type"] == "image").unwrap();
        assert_eq!(img_block["source"]["type"], "base64");
        assert_eq!(img_block["source"]["media_type"], "image/png");
        assert_eq!(img_block["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn test_system_message_separated() {
        let msgs = vec![
            Message::system("You are a helpful assistant."),
            Message::user("Hello"),
        ];
        let config = LlmConfig::default();
        let req = build_anthropic_request(&msgs, &[], &config);
        assert_eq!(
            req["system"],
            serde_json::json!([{
                "type": "text",
                "text": "You are a helpful assistant.",
                "cache_control": { "type": "ephemeral" }
            }])
        );
        let msgs_arr = req["messages"].as_array().unwrap();
        assert_eq!(msgs_arr.len(), 1);
        assert_eq!(msgs_arr[0]["role"], "user");
    }

    #[test]
    fn test_tool_message_creates_new_user() {
        let msgs = vec![
            Message::user("Check the weather"),
            Message::assistant("Let me look that up"),
            Message::tool("Sunny 22°C", "toolu_abc123"),
        ];
        let config = LlmConfig::default();
        let req = build_anthropic_request(&msgs, &[], &config);
        let msgs_arr = req["messages"].as_array().unwrap();
        // tool 消息创建独立 user 消息，3 条：user, assistant, user(tool_result)
        assert_eq!(msgs_arr.len(), 3);
        assert_eq!(msgs_arr[0]["role"], "user");
        assert_eq!(msgs_arr[1]["role"], "assistant");
        assert_eq!(msgs_arr[2]["role"], "user");
        let content = msgs_arr[2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_abc123");
        assert_eq!(content[0]["content"], "Sunny 22°C");
    }

    #[test]
    fn test_consecutive_same_role_merged() {
        let msgs = vec![
            Message::user("Hello"),
            Message::assistant("First response"),
            Message::assistant("Second response"),
        ];
        let config = LlmConfig::default();
        let req = build_anthropic_request(&msgs, &[], &config);
        let msgs_arr = req["messages"].as_array().unwrap();
        assert_eq!(msgs_arr.len(), 2);
        assert_eq!(msgs_arr[1]["role"], "assistant");
        let content = msgs_arr[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "First response\n\nSecond response");
    }

    // ── byte_stream_to_chat_events 集成测试 ──────────────────────────

    /// 构造一条 Anthropic SSE 文本行（自动追加 \n\n）
    fn sse_line(event: &str, data: &str) -> String {
        format!("event: {}\ndata: {}\n\n", event, data)
    }

    /// 收集 ChatEvent 流到 Vec（使用 Instant::now 作为 start_time）
    async fn collect_anthropic_events(chunks: Vec<String>, project_path: &str) -> Vec<ChatEvent> {
        let byte_stream =
            futures::stream::iter(chunks.into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let span = tracing::Span::current();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            project_path.to_string(),
            false,
            20000,
            true,
        );
        event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await
    }

    #[tokio::test]
    async fn test_anthropic_response_carries_metadata() {
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":125}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], "/tmp").await;

        // 提取 OutputMetadata
        let metadata_events: Vec<&ChatEvent> = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::OutputMetadata(_)))
            .collect();
        assert_eq!(
            metadata_events.len(),
            1,
            "should have exactly one OutputMetadata event"
        );

        let meta = &metadata_events[0];
        if let ChatEvent::OutputMetadata(m) = meta {
            assert_eq!(m.model, "claude-sonnet-4-6");
            assert_eq!(m.finish_reasons, vec!["end_turn"]);
            assert_eq!(m.input_tokens, 105);
            assert_eq!(m.output_tokens, 125);
            assert_eq!(m.cache_read_input_tokens, None);
            assert_eq!(m.cache_creation_input_tokens, None);
            // latency_ms is u64, always non-negative
        } else {
            panic!("expected OutputMetadata");
        }

        // 验证事件顺序：UsageDelta → TextDelta → UsageDelta → UsageInfo → OutputMetadata → Done
        // （message_start / message_delta 携带非零 usage，各自先发 UsageDelta 供实时速率展示）
        let type_names: Vec<&str> = events
            .iter()
            .map(|e| match e {
                ChatEvent::TextDelta(_) => "TextDelta",
                ChatEvent::UsageDelta { .. } => "UsageDelta",
                ChatEvent::UsageInfo { .. } => "UsageInfo",
                ChatEvent::OutputMetadata(_) => "OutputMetadata",
                ChatEvent::Done => "Done",
                _ => "Other",
            })
            .collect();
        assert_eq!(
            type_names,
            vec![
                "UsageDelta",
                "TextDelta",
                "UsageDelta",
                "UsageInfo",
                "OutputMetadata",
                "Done"
            ],
            "event order should be UsageDelta → TextDelta → UsageDelta → UsageInfo → OutputMetadata → Done"
        );
    }

    #[tokio::test]
    async fn test_anthropic_cache_tokens_extracted() {
        // message_start 包含 cache tokens
        let message_start = r#"{"type":"message_start","message":{"id":"msg_02","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200,"output_tokens":0,"cache_creation_input_tokens":80,"cache_read_input_tokens":300}}}"#;
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], "/tmp").await;

        // 提取 OutputMetadata
        let meta = events
            .iter()
            .find_map(|e| {
                if let ChatEvent::OutputMetadata(m) = e {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .expect("should have OutputMetadata event");

        assert_eq!(meta.model, "claude-sonnet-4-5");
        assert_eq!(meta.input_tokens, 200);
        assert_eq!(meta.output_tokens, 50);
        assert_eq!(meta.cache_read_input_tokens, Some(300));
        assert_eq!(meta.cache_creation_input_tokens, Some(80));
        assert_eq!(meta.finish_reasons, vec!["end_turn"]);
    }

    /// 逐事件上报 UsageDelta（message_start 的 input/cache 增量 + message_delta 的
    /// output 增量），最终 UsageInfo 仍取流末累计总量，不被增量重复累加。
    #[tokio::test]
    async fn test_anthropic_usage_delta_emitted_and_totals_settle() {
        let message_start = r#"{"type":"message_start","message":{"id":"msg_04","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":20,"cache_read_input_tokens":10}}}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":30}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}",
            sse_line("message_start", message_start),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], "/tmp").await;

        // UsageDelta 序列：message_start 上报 input/cache 增量，message_delta 上报 output 增量
        let deltas: Vec<(u32, u32, u32, u32)> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::UsageDelta {
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                } => Some((
                    *input_tokens,
                    *output_tokens,
                    *cache_creation_input_tokens,
                    *cache_read_input_tokens,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec![(100, 0, 20, 10), (0, 30, 0, 0)],
            "每个 usage 事件的增量（相对已上报值求差分）"
        );

        // 结算总量仍与改动前一致：input 100 / output 30 / cache_creation 20 / cache_read 10
        let usage = events.iter().find_map(|e| match e {
            ChatEvent::UsageInfo {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                ..
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_creation_input_tokens,
                *cache_read_input_tokens,
            )),
            _ => None,
        });
        assert_eq!(
            usage,
            Some((100, 30, 20, 10)),
            "结算总量取流末累计值，不被 UsageDelta 重复累加"
        );
    }

    #[tokio::test]
    async fn test_anthropic_latency_ms_recorded() {
        // 人为延迟 ≥ 20ms 后创建流，断言 latency_ms >= 20
        let message_start = r#"{"type":"message_start","message":{"id":"msg_03","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        // 先在 start_time 前 sleep，确保延迟包含在测量中
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let sse = format!(
            "{}{}{}",
            sse_line("message_start", message_start),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let span = tracing::Span::current();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let events: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        let meta = events
            .iter()
            .find_map(|e| {
                if let ChatEvent::OutputMetadata(m) = e {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .expect("should have OutputMetadata event");

        assert!(
            meta.latency_ms >= 20,
            "latency_ms ({}) should be >= 20",
            meta.latency_ms
        );
    }

    // --- 图片内容块测试 ---

    /// 生成独立的临时项目目录（用于保存图片）
    fn test_image_project_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "visp_anthropic_image_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn test_parse_image_base64() {
        let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        match result {
            ParsedEvent::ImageBlock {
                data,
                mime_type,
                remote_url,
            } => {
                assert_eq!(data.as_deref(), Some("iVBORw0KGgo="));
                assert_eq!(mime_type, "image/png");
                assert!(remote_url.is_none());
            }
            _ => panic!("expected ImageBlock, got {:?}", result),
        }
    }

    #[test]
    fn test_parse_image_url() {
        let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"url","url":"https://example.com/image.png"}}}"#;
        let result = parse_anthropic_event("content_block_start", data).unwrap();
        match result {
            ParsedEvent::ImageBlock {
                data,
                mime_type,
                remote_url,
            } => {
                assert!(data.is_none());
                assert!(mime_type.is_empty());
                assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
            }
            _ => panic!("expected ImageBlock, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn test_byte_stream_base64_image() {
        let project_path = test_image_project_dir("base64_image");
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":1}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_stop", block_stop),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], &project_path).await;

        let image_events: Vec<&ChatEvent> = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::ImageBlock { .. }))
            .collect();
        assert_eq!(image_events.len(), 1, "should have one ImageBlock event");
        if let ChatEvent::ImageBlock {
            path,
            mime_type,
            remote_url,
        } = image_events[0]
        {
            assert!(!path.is_empty(), "base64 image should be saved to disk");
            let saved = std::path::Path::new(path);
            assert_eq!(
                saved.parent().and_then(std::path::Path::file_name),
                Some(std::ffi::OsStr::new("images")),
                "image should be saved under .visp/images/, got {path}"
            );
            assert_eq!(
                saved
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name),
                Some(std::ffi::OsStr::new(".visp")),
                "image should be saved under a project's .visp directory, got {path}"
            );
            assert_eq!(mime_type, "image/png");
            assert!(remote_url.is_none());
            // 文件确实写入磁盘
            assert!(saved.exists(), "saved image file should exist");
        } else {
            panic!("expected ImageBlock event");
        }
    }

    #[tokio::test]
    async fn test_byte_stream_url_image() {
        let project_path = test_image_project_dir("url_image");
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"url","url":"https://example.com/image.png"}}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":1}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_stop", block_stop),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], &project_path).await;

        let image_events: Vec<&ChatEvent> = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::ImageBlock { .. }))
            .collect();
        assert_eq!(image_events.len(), 1, "should have one ImageBlock event");
        if let ChatEvent::ImageBlock {
            path,
            mime_type,
            remote_url,
        } = image_events[0]
        {
            assert!(path.is_empty(), "URL image should not be saved to disk");
            assert!(mime_type.is_empty());
            assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
        } else {
            panic!("expected ImageBlock event");
        }
    }

    #[tokio::test]
    async fn test_byte_stream_base64_error() {
        let project_path = test_image_project_dir("base64_error");
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"!!!invalid-base64!!!"}}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":1}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_stop", block_stop),
            sse_line("message_stop", message_stop),
        );
        let events = collect_anthropic_events(vec![sse], &project_path).await;

        let error_events: Vec<&ChatEvent> = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::ImageError { .. }))
            .collect();
        assert_eq!(error_events.len(), 1, "should have one ImageError event");
        if let ChatEvent::ImageError { reason } = error_events[0] {
            assert!(
                reason.contains("base64"),
                "error reason should mention base64, got {reason}"
            );
        } else {
            panic!("expected ImageError event");
        }
    }

    // --- UTF-8 跨 chunk 边界测试 ---

    /// 收集 ChatEvent 流，接收字节 chunk（用于测试跨 chunk 的 UTF-8 切分）
    async fn collect_anthropic_events_from_bytes(
        chunks: Vec<Vec<u8>>,
        project_path: &str,
    ) -> Vec<ChatEvent> {
        let byte_stream =
            futures::stream::iter(chunks.into_iter().map(|b| Ok(bytes::Bytes::from(b))));
        let start = std::time::Instant::now();
        let span = tracing::Span::current();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            project_path.to_string(),
            false,
            20000,
            true,
        );
        event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await
    }

    #[tokio::test]
    async fn test_anthropic_utf8_multibyte_split_across_chunks() {
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"之间"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let full_bytes = sse.into_bytes();
        let zhishi = "之间".as_bytes();
        let split_offset = full_bytes
            .windows(zhishi.len())
            .position(|w| w == zhishi)
            .expect("should find 之间 in SSE");
        // 在 "之" 的第 2 字节后切分：e4 b9 | 8b e9 97 b4
        let cut = split_offset + 2;
        let chunk1 = full_bytes[..cut].to_vec();
        let chunk2 = full_bytes[cut..].to_vec();

        let events = collect_anthropic_events_from_bytes(vec![chunk1, chunk2], "/tmp").await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            text, "之间",
            "Anthropic Chinese text should survive chunk split"
        );
        assert!(
            !text.contains('\u{FFFD}'),
            "no U+FFFD replacement char allowed"
        );
    }

    #[tokio::test]
    async fn test_anthropic_utf8_multibyte_split_at_char_boundary() {
        let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"之间"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        let sse = format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let full_bytes = sse.into_bytes();
        let zhi = "之".as_bytes();
        let split_offset = full_bytes
            .windows(zhi.len())
            .position(|w| w == zhi)
            .expect("should find 之 in SSE");
        let cut = split_offset + zhi.len();
        let chunk1 = full_bytes[..cut].to_vec();
        let chunk2 = full_bytes[cut..].to_vec();

        let events = collect_anthropic_events_from_bytes(vec![chunk1, chunk2], "/tmp").await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "之间");
        assert!(!text.contains('\u{FFFD}'));
    }

    // ── Tracing / gen_ai.client.operation span tests ────────────────────────

    use std::sync::{Arc, Mutex};
    use tracing::Event;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        name: String,
        fields: Vec<(String, String)>,
        id: u64,
    }

    struct SpanFieldVisitor {
        fields: Vec<(String, String)>,
    }

    impl Visit for SpanFieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    struct TestLayer {
        spans: Arc<Mutex<Vec<CapturedSpan>>>,
        events: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl TestLayer {
        fn new(
            spans: Arc<Mutex<Vec<CapturedSpan>>>,
            events: Arc<Mutex<Vec<(String, String)>>>,
        ) -> Self {
            Self { spans, events }
        }
    }

    impl<S> Layer<S> for TestLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
            let mut visitor = SpanFieldVisitor { fields: Vec::new() };
            attrs.record(&mut visitor);
            self.spans.lock().unwrap().push(CapturedSpan {
                name: attrs.metadata().name().to_string(),
                fields: visitor.fields,
                id: id.into_u64(),
            });
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
            let mut visitor = SpanFieldVisitor { fields: Vec::new() };
            values.record(&mut visitor);
            let mut spans = self.spans.lock().unwrap();
            if let Some(span) = spans.iter_mut().find(|s| s.id == id.into_u64()) {
                span.fields.extend(visitor.fields);
            }
        }

        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let target = event.metadata().target().to_string();
            let name = event.metadata().name().to_string();
            self.events.lock().unwrap().push((target, name));
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup_tracing() -> (
        Arc<Mutex<Vec<CapturedSpan>>>,
        Arc<Mutex<Vec<(String, String)>>>,
    ) {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        (spans, events)
    }

    fn make_guard(
        spans: &Arc<Mutex<Vec<CapturedSpan>>>,
        events: &Arc<Mutex<Vec<(String, String)>>>,
    ) -> tracing::subscriber::DefaultGuard {
        tracing_subscriber::registry()
            .with(TestLayer::new(spans.clone(), events.clone()))
            .set_default()
    }

    #[test]
    fn test_gen_ai_client_operation_span_created() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        // 模拟 byte_stream_to_chat_events 传 span 的处理
        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = "claude-sonnet-4",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = "anthropic",
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            visp.llm.token_limit_hit = tracing::field::Empty,
        );
        span.record("gen_ai.request.max_tokens", 4096i64);
        span.record("gen_ai.request.temperature", 0.7f64);

        drop(_guard);
        let spans = spans.lock().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "gen_ai.client.operation");
    }

    #[test]
    fn test_gen_ai_request_fields_at_span_start() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = "claude-sonnet-4",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = "anthropic",
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            visp.llm.token_limit_hit = tracing::field::Empty,
        );

        // 模拟 chat_stream 中在 span 创建后的 record
        span.record("gen_ai.system", "anthropic");
        span.record("gen_ai.request.max_tokens", 4096i64);
        span.record("gen_ai.request.temperature", 0.7f64);

        drop(_guard);
        let spans = spans.lock().unwrap();
        assert_eq!(spans.len(), 1);
        let fields = &spans[0].fields;
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.request.model" && v == "claude-sonnet-4")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.operation.name" && v == "chat")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "visp.llm.attempt" && v == "0")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.system" && v == "anthropic")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.provider.name" && v == "anthropic")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.request.max_tokens" && v == "4096")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.request.temperature" && v == "0.7")
        );
    }

    #[test]
    fn test_max_tokens_field_aligned_with_anthropic_api() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.request.max_tokens = tracing::field::Empty,
        );
        span.record("gen_ai.request.max_tokens", 8192u64);

        drop(_guard);
        let spans = spans.lock().unwrap();
        // 验证字段名为 gen_ai.request.max_tokens（不是 max_output_tokens）
        let has_field = spans[0]
            .fields
            .iter()
            .any(|(k, v)| k == "gen_ai.request.max_tokens" && v == "8192");
        assert!(has_field, "field name should be gen_ai.request.max_tokens");
    }

    /// 生成一组完整的 SSE 事件，用于测试 usage 字段记录
    fn make_complete_sse(model: &str, stop_reason: &str) -> String {
        let message_start = format!(
            r#"{{"type":"message_start","message":{{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"{}","stop_reason":null,"stop_sequence":null,"usage":{{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":52,"cache_read_input_tokens":200}}}}}}"#,
            model
        );
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{}","stop_sequence":null}},"usage":{{"output_tokens":420}}}}"#,
            stop_reason
        );
        let message_stop = r#"{"type":"message_stop"}"#;

        fn sse_line(event: &str, data: &str) -> String {
            format!("event: {}\ndata: {}\n\n", event, data)
        }

        format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", &message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", &message_delta),
            sse_line("message_stop", message_stop),
        )
    }

    #[tokio::test]
    async fn test_gen_ai_usage_fields_recorded_on_completion() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.system = tracing::field::Empty,
            gen_ai.request.model = "claude-sonnet-4-6",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = "anthropic",
            gen_ai.request.max_tokens = tracing::field::Empty,
            gen_ai.request.temperature = tracing::field::Empty,
            visp.llm.attempt = 0u64,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            visp.llm.token_limit_hit = tracing::field::Empty,
        );
        span.record("gen_ai.request.max_tokens", 4096i64);
        span.record("gen_ai.request.temperature", 0.7f64);

        let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _events: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        // Wait for stream completion
        drop(_guard);

        let spans = spans.lock().unwrap();
        assert_eq!(spans.len(), 1);
        let fields = &spans[0].fields;

        // 验证 usage 字段
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.input_tokens" && v == "105")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.output_tokens" && v == "420")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.cache_read.input_tokens" && v == "200")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.cache_creation.input_tokens" && v == "52")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"end_turn\"]")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.response.model" && v == "claude-sonnet-4-6")
        );
    }

    #[test]
    fn test_finish_reasons_serialized_as_json_array() {
        // 验证 JSON 数组序列化
        let reasons = vec!["end_turn".to_string(), "stop".to_string()];
        let serialized = serde_json::to_string(&reasons).unwrap();
        assert_eq!(serialized, "[\"end_turn\",\"stop\"]");

        let single = vec!["end_turn".to_string()];
        assert_eq!(serde_json::to_string(&single).unwrap(), "[\"end_turn\"]");

        let empty: Vec<String> = vec![];
        assert_eq!(serde_json::to_string(&empty).unwrap(), "[]");
    }

    #[test]
    fn test_gen_ai_client_retry_event_emitted() {
        let (_spans, events) = setup_tracing();
        let _guard = make_guard(&_spans, &events);

        // 模拟重试事件
        tracing::warn!(
            target: "gen_ai.client.retry",
            reason = "rate_limit",
            "retrying LLM request"
        );

        drop(_guard);
        let evts = events.lock().unwrap();
        assert!(
            evts.iter().any(|(t, _)| t == "gen_ai.client.retry"),
            "should find retry event"
        );
    }

    #[tokio::test]
    async fn test_gen_ai_client_first_token_event() {
        let (spans, events) = setup_tracing();
        let _guard = make_guard(&spans, &events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
        );

        let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);

        let evts = events.lock().unwrap();
        assert!(
            evts.iter().any(|(t, _)| t == "gen_ai.client.first_token"),
            "should find first_token event"
        );
    }

    #[test]
    fn test_gen_ai_provider_name_is_anthropic() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let _span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.provider.name = "anthropic",
            gen_ai.operation.name = "chat",
        );

        drop(_guard);
        let spans = spans.lock().unwrap();
        assert!(
            spans[0]
                .fields
                .iter()
                .any(|(k, v)| k == "gen_ai.provider.name" && v == "anthropic")
        );
    }

    #[tokio::test]
    async fn test_anthropic_finish_reason_max_tokens_normalized_to_length() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.response.finish_reasons = tracing::field::Empty,
            visp.llm.token_limit_hit = tracing::field::Empty,
        );

        // 模拟 max_tokens stop_reason
        let sse = make_complete_sse("claude-sonnet-4-6", "max_tokens");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);
        let spans = spans.lock().unwrap();
        let fields = &spans[0].fields;

        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"length\"]"),
            "max_tokens should be normalized to length, got: {:?}",
            fields
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "visp.llm.token_limit_hit" && v == "true"),
            "token_limit_hit should be true for max_tokens"
        );
    }

    #[test]
    fn test_anthropic_finish_reason_end_turn_stays_unmodified() {
        let reason = crate::anthropic::normalize_anthropic_reason("end_turn");
        assert_eq!(reason, "end_turn");
    }

    #[test]
    fn test_anthropic_finish_reason_tool_use_stays_unmodified() {
        let reason = crate::anthropic::normalize_anthropic_reason("tool_use");
        assert_eq!(reason, "tool_use");
    }

    #[test]
    fn test_anthropic_finish_reason_unknown_stays_unmodified() {
        let reason = crate::anthropic::normalize_anthropic_reason("unknown_reason");
        assert_eq!(reason, "unknown_reason");
    }

    #[tokio::test]
    async fn test_anthropic_token_limit_hit_when_max_tokens() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.response.finish_reasons = tracing::field::Empty,
            visp.llm.token_limit_hit = tracing::field::Empty,
        );

        let sse = make_complete_sse("claude-sonnet-4-6", "max_tokens");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);
        let spans = spans.lock().unwrap();
        assert!(
            spans[0]
                .fields
                .iter()
                .any(|(k, v)| k == "visp.llm.token_limit_hit" && v == "true"),
            "token_limit_hit should be true for max_tokens"
        );
    }

    #[tokio::test]
    async fn test_anthropic_token_limit_not_set_for_normal_stop() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            visp.llm.token_limit_hit = tracing::field::Empty,
        );

        let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);
        let spans = spans.lock().unwrap();
        let token_limit_entry = spans[0]
            .fields
            .iter()
            .find(|(k, _)| k == "visp.llm.token_limit_hit");
        assert!(
            token_limit_entry.is_none() || token_limit_entry.unwrap().1 == "false",
            "token_limit_hit should not be true for normal stop"
        );
    }

    #[tokio::test]
    async fn test_anthropic_cache_uses_new_dot_notation() {
        let (spans, _events) = setup_tracing();
        let _guard = make_guard(&spans, &_events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
        );

        let message_start = r#"{"type":"message_start","message":{"id":"msg_02","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200,"output_tokens":0,"cache_creation_input_tokens":80,"cache_read_input_tokens":300}}}"#;
        let block_start =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let text_delta =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
        let block_stop = r#"{"type":"content_block_stop","index":0}"#;
        let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50}}"#;
        let message_stop = r#"{"type":"message_stop"}"#;

        fn sse_line(event: &str, data: &str) -> String {
            format!("event: {}\ndata: {}\n\n", event, data)
        }

        let sse = format!(
            "{}{}{}{}{}{}",
            sse_line("message_start", message_start),
            sse_line("content_block_start", block_start),
            sse_line("content_block_delta", text_delta),
            sse_line("content_block_stop", block_stop),
            sse_line("message_delta", message_delta),
            sse_line("message_stop", message_stop),
        );
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);
        let spans = spans.lock().unwrap();
        let fields = &spans[0].fields;

        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.cache_read.input_tokens" && v == "300")
        );
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "gen_ai.usage.cache_creation.input_tokens" && v == "80")
        );
        // 确认旧的下划线字段名不存在
        assert!(
            !fields
                .iter()
                .any(|(k, _)| k == "gen_ai.usage.cache_read_input_tokens")
        );
        assert!(
            !fields
                .iter()
                .any(|(k, _)| k == "gen_ai.usage.cache_creation_input_tokens")
        );
    }

    #[tokio::test]
    async fn test_gen_ai_client_completed_event() {
        let (spans, events) = setup_tracing();
        let _guard = make_guard(&spans, &events);

        let span = tracing::info_span!(
            "gen_ai.client.operation",
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
        );

        let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
        let byte_stream =
            futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
        let start = std::time::Instant::now();
        let event_stream = byte_stream_to_chat_events(
            byte_stream,
            start,
            span,
            "test-model".to_string(),
            "/tmp".to_string(),
            false,
            20000,
            true,
        );
        let _: Vec<ChatEvent> = event_stream
            .filter_map(|e| futures::future::ready(e.ok()))
            .collect()
            .await;

        drop(_guard);

        let evts = events.lock().unwrap();
        assert!(
            evts.iter().any(|(t, _)| t == "gen_ai.client.completed"),
            "should find completed event; found: {:?}",
            evts,
        );
    }

    // --- build_anthropic_request thinking 合规测试 ---

    fn make_anthropic_config(
        temp: f64,
        max_tokens: u32,
        thinking_budget: Option<&str>,
    ) -> LlmConfig {
        let mut config = LlmConfig {
            model: "glm-5.2".into(),
            temperature: temp,
            max_tokens,
            ..Default::default()
        };
        if let Some(b) = thinking_budget {
            config
                .extra
                .insert("thinking_budget_tokens".into(), b.into());
        }
        config
    }

    #[test]
    fn test_anthropic_thinking_forces_temperature_one() {
        let config = make_anthropic_config(0.7, 8000, Some("12800"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["temperature"].as_f64(), Some(1.0));
    }

    #[test]
    fn test_anthropic_thinking_budget_clamped_when_exceeds_max_tokens() {
        let config = make_anthropic_config(0.7, 8000, Some("12800"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(7999));
    }

    #[test]
    fn test_anthropic_thinking_budget_clamped_when_equals_max_tokens() {
        let config = make_anthropic_config(0.7, 8000, Some("8000"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(7999));
    }

    #[test]
    fn test_anthropic_thinking_budget_kept_when_below_max_tokens() {
        let config = make_anthropic_config(0.7, 8000, Some("4096"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(4096));
    }

    #[test]
    fn test_anthropic_no_thinking_keeps_temperature() {
        let config = make_anthropic_config(0.7, 8000, None);
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["temperature"].as_f64(), Some(0.7));
        assert!(req.get("thinking").is_none());
    }

    #[test]
    fn test_anthropic_thinking_invalid_budget_skipped() {
        let config = make_anthropic_config(0.7, 8000, Some("not-a-number"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert!(req.get("thinking").is_none());
        assert_eq!(req["temperature"].as_f64(), Some(0.7));
    }

    #[test]
    fn test_anthropic_thinking_skipped_when_max_tokens_zero() {
        let config = make_anthropic_config(0.7, 0, Some("12800"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert!(req.get("thinking").is_none());
        assert_eq!(req["temperature"].as_f64(), Some(0.7));
    }

    #[test]
    fn test_anthropic_thinking_temperature_idempotent() {
        let config = make_anthropic_config(1.0, 8000, Some("4096"));
        let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
        assert_eq!(req["temperature"].as_f64(), Some(1.0));
    }
}
