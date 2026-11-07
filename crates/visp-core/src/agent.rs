// Allow unused imports — they are consumed by test module via `use super::*`
#![allow(unused_imports)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::agent_definition::PermissionRule;
use crate::error::AgentErrorCode;
use crate::error::LlmError;
use crate::message::Message;
use crate::message::Role;
use crate::message::ToolCallRequest;
use crate::message::estimate_message_tokens;
use crate::prompt::PromptBuilder;
use crate::provider::ChatEvent;
use crate::provider::LlmConfig;
use crate::provider::LlmProvider;
use crate::rules::RuleEngine;
use crate::session::SessionManager;
use crate::session::SessionStatus;
use crate::tool::{ToolContext, ToolResult, ToolType};
use crate::tool_registry::ToolRegistry;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use visp_observability::LangfuseSpanConfig;

/// 用户查询结果
#[derive(Debug, Clone, Default)]
pub struct UserQueryResult {
    pub selected_index: i32,
    pub text: String,
}

/// `AgentEvent::UserQuery` 的显式类型：区分「工具审批弹窗」与「普通提问」。
///
/// 冻结契约 `PermissionRequest.kind: {approval, question}` 要求由事件**显式携带**，
/// 避免消费方对 `message` 做字符串嗅探（设计 §6.2 D12）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    /// 工具审批弹窗（`requires_approval` 路径）。
    Approval,
    /// 普通提问（LLM 输出的 `[USER_QUERY]` 标记路径）。
    Question,
}

/// 工具调用结果分类。
///
/// 作为 `AgentEvent::ToolCallResult` 的显式状态字段，并承担 `is_error` 的
/// **单一映射来源**（见 [`ToolOutcome::into_is_error`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// 工具成功执行
    Success,
    /// 工具执行返回失败
    Failure,
    /// 审批被用户拒绝
    Denied,
    /// 执行前已取消
    Cancelled,
    /// 工具参数畸形/截断（如 max_output_tokens 超限）
    Truncated,
}

impl ToolOutcome {
    /// `outcome → is_error` 的单一映射：仅 `Success` 为 `false`，其余为 `true`。
    pub fn into_is_error(self) -> bool {
        !matches!(self, ToolOutcome::Success)
    }
}

/// Agent 事件，用于流式通知外部（TUI/WS）
#[derive(Clone)]
pub enum AgentEvent {
    /// 文本增量
    TextDelta(String),
    /// 思考块（如 DeepSeek thinking mode）
    ///
    /// 承载 provider 下发的**累积快照**（截至当前的完整思考文本）。流式路径已不再
    /// 发射本变体，仅保留用于兼容既有匹配臂；新增下游请使用
    /// [`AgentEvent::ThinkingDelta`]。
    ThinkingBlock(serde_json::Value),
    /// 思考文本的**真增量**（相对同一轮 LLM 响应内上一帧快照的差集尾部）。
    ///
    /// provider/core 内部契约仍是累积快照，本变体由 core 的流收集阶段换算而来，
    /// 只承载新增文本，供 ACP 等消费方按「同 messageId 追加」消费，避免把快照
    /// 当增量重复追加导致的 O(n²) 复读。发射点唯一：流式路径只发本变体。
    ThinkingDelta(String),
    /// token 用量及工具调用统计
    UsageInfo {
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// 本次请求的累计成本（provider 提供时才有值）
        cost: Option<f64>,
    },
    /// 逐 chunk 的 token 增量（用于实时速率显示，不参与最终总量结算）
    UsageDelta {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
    /// 工具调用请求
    ToolCallRequest {
        call_id: String,
        tool_name: String,
        arguments: String,
    },
    /// 工具调用结果
    ToolCallResult {
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        /// 结果分类（`is_error` 由此单一映射，见 `ToolOutcome::into_is_error`）
        outcome: ToolOutcome,
    },
    /// 工具即将执行（审批通过后、真实执行前）。
    ///
    /// 仅类型定义；发射点由后续 hooks 实现步骤接入。
    PreToolUse {
        call_id: String,
        tool_name: String,
        requires_approval: bool,
    },
    /// 状态更新
    StatusUpdate(String),
    /// 发生错误
    Error {
        code: AgentErrorCode,
        message: String,
    },
    /// 完成
    Done,
    /// LLM 输出的图片内容块
    ImageBlock {
        path: String,
        mime_type: String,
        remote_url: Option<String>,
    },
    /// 图片处理失败
    ImageError { reason: String },
    /// 需要用户输入
    UserQuery {
        query_id: String,
        message: String,
        options: Vec<String>,
        allow_other: bool,
        /// 显式类型：审批弹窗 vs 普通提问（见 [`PermissionKind`]）。
        kind: PermissionKind,
        respond: mpsc::Sender<UserQueryResult>,
    },
}

/// Agent 事件帧：AgentEvent 及其来源上下文。
/// 用于标识事件来自哪个 agent，支持 CLI 显示 agent 名称前缀。
#[derive(Clone)]
pub struct AgentEventFrame {
    pub event: AgentEvent,
    pub session_id: String,
    pub agent_name: String,
    pub parent_session_id: Option<String>,
    pub parent_session_name: Option<String>,
}

/// Agent → Orchestrator 消息（通过全局事件总线）
pub enum AgentMessage {
    TextDelta(String),
    ThinkingBlock(serde_json::Value),
    UsageInfo {
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        /// 本次请求的累计成本（provider 提供时才有值）
        cost: Option<f64>,
    },
    /// 逐 chunk 的 token 增量（用于实时速率显示，不参与最终总量结算）
    UsageDelta {
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
    StatusUpdate(String),
    Error {
        code: AgentErrorCode,
        message: String,
    },
    ToolCallRequest {
        call_id: String,
        tool_name: String,
        arguments: String,
    },
    ToolCallResult {
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    UserQuery {
        query_id: String,
        message: String,
        options: Vec<String>,
        allow_other: bool,
        respond: mpsc::Sender<UserQueryResult>,
    },
    SpawnRequest {
        call_id: String,
        subagent_type: String,
        description: String,
        /// 传给子 agent 的详细、自包含任务说明（目标+上下文+约束+期望输出）。
        /// 由调用方 LLM 编写，子 agent 将其作为首条 user message。
        prompt: String,
        task_id: Option<String>,
        trace_context: Option<crate::TraceContext>,
        /// 用于将子 agent 的响应发送回调用方
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    },
    Done,
}

/// Orchestrator → Agent 消息（通过专属 inbox）
pub enum OrchestratorMessage {
    SubAgentComplete {
        call_id: String,
        content: String,
        task_id: String,
    },
    SubAgentError {
        call_id: String,
        error: String,
    },
    Cancelled,
}

/// 事件总线信封
pub struct Envelope {
    pub session_id: String,
    pub message: AgentMessage,
    pub trace_context: Option<crate::TraceContext>,
}

/// Agent kind (primary vs sub).
///
/// Used for observability fields (`visp.agent.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// Root agent handling the user's session.
    Primary,
    /// Sub-agent spawned by a parent agent.
    Sub,
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentKind::Primary => write!(f, "primary"),
            AgentKind::Sub => write!(f, "sub"),
        }
    }
}

/// A tool that delegates a task to a sub-agent.
/// Created dynamically for each sub-agent in the AgentRegistry.
pub struct AgentTool {
    agent_name: String,
    agent_description: String,
}

impl AgentTool {
    pub fn new(name: String, description: String) -> Self {
        Self {
            agent_name: name,
            agent_description: description,
        }
    }
}

#[async_trait]
impl crate::tool::Tool for AgentTool {
    fn name(&self) -> &str {
        &self.agent_name
    }

    fn description(&self) -> &str {
        &self.agent_description
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The detailed, self-contained task to delegate to the sub-agent"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value, context: &ToolContext) -> ToolResult {
        let global_tx = match &context.global_tx {
            Some(tx) => tx.clone(),
            None => {
                return ToolResult::error(
                    "[SubAgent Error] Agent tool requires multi-agent mode (global_tx not available)",
                );
            }
        };

        let prompt = match arguments.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return ToolResult::error("[SubAgent Error] Missing required 'prompt' argument");
            }
        };

        let (response_tx, response_rx) = oneshot::channel::<String>();

        let trace_context = context.visp_trace_id.as_ref().and_then(|trace_id| {
            context.iter_span_w3c_id.as_ref().and_then(|span_id| {
                crate::TraceContext::new(trace_id.clone(), span_id.clone(), 1, None, None).ok()
            })
        });

        let envelope = Envelope {
            session_id: context.session_id.clone().unwrap_or_default(),
            message: AgentMessage::SpawnRequest {
                call_id: uuid::Uuid::new_v4().to_string(),
                subagent_type: self.agent_name.clone(),
                description: self.agent_description.clone(),
                prompt,
                task_id: None,
                trace_context: trace_context.clone(),
                response_tx: Some(response_tx),
            },
            trace_context,
        };

        if let Err(e) = global_tx.send(envelope).await {
            return ToolResult::error(format!(
                "[SubAgent Error] Failed to send spawn request: {e}"
            ));
        }

        wait_subagent_response(response_rx, SUBAGENT_RESPONSE_TIMEOUT).await
    }

    fn category(&self) -> &str {
        "agent"
    }

    fn tool_type(&self) -> ToolType {
        ToolType::Agent
    }
}

/// 等待子代理响应，超时返回错误（防止 orchestrator 未处理时永久挂起）。
const SUBAGENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1800);

async fn wait_subagent_response(
    response_rx: oneshot::Receiver<String>,
    timeout: Duration,
) -> ToolResult {
    match tokio::time::timeout(timeout, response_rx).await {
        Ok(Ok(content)) => ToolResult::success(content),
        Ok(Err(_)) => {
            ToolResult::error("[SubAgent Error] Sub-agent response channel closed unexpectedly")
        }
        Err(_) => ToolResult::error("[SubAgent Error] Sub-agent response timed out"),
    }
}

/// Agent 循环上下文
pub struct AgentLoopContext {
    /// 会话 ID
    pub session_id: String,
    /// 对话历史
    pub history: Vec<Message>,
    /// 工作目录
    pub working_dir: PathBuf,
    /// LLM 配置
    pub config: LlmConfig,
    /// 取消令牌
    pub cancel_token: CancellationToken,
    /// 上下文裁剪器
    pub context_trimmer: Arc<dyn crate::context::ContextTrimmer + Send + Sync>,
    /// 全局事件总线发送端（多 Agent 模式）
    pub global_tx: Option<mpsc::Sender<Envelope>>,
    /// 权限规则集
    pub permission_rules: Option<Arc<Vec<PermissionRule>>>,
    /// Agent kind (Primary or Sub), used for observability.
    pub agent_kind: AgentKind,
    /// Nesting depth (0 for root, incremented per sub-agent level).
    pub depth: u32,
    /// 父 agent 的 session_id；主 agent 为自身 session_id（自引用）
    pub parent_session_id: String,
}

/// Agent 执行配置
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 软上限：达到此轮次后在 LLM 调用前注入"请收尾"提示（0 = 关闭）
    pub soft_limit: u32,
    /// 硬上限兜底：达到此轮次后强制终止（防止死循环）
    pub hard_limit: u32,
    /// Doom loop 检测窗口：连续 N 轮相同工具调用则触发警告
    pub doom_loop_threshold: u32,
    /// LLM 调用重试次数
    pub llm_retry_attempts: u32,
    /// LLM 重试基础延迟（毫秒）
    pub llm_retry_base_delay_ms: u64,
    /// bash 工具确认模式（执行高危命令前是否需要用户确认）
    pub bash_confirm_mode: bool,
    /// 文件读取/写入的最大字节数
    pub file_max_size_bytes: u64,
    /// Agent 嵌套深度上限
    pub max_depth: u32,
    /// 全局同时运行的 subagent 数量上限（主 agent 不计入，超出排队等待）
    pub max_concurrent_subagents: u32,
    /// Langfuse 总开关
    pub langfuse_enabled: bool,
    /// Langfuse user.id 字段值（None = 不设置）
    pub langfuse_user_id: Option<String>,
    /// Langfuse tags 字段值（JSON 字符串，None = 不设置）
    pub langfuse_tags: Option<String>,
    /// Langfuse environment 字段
    pub langfuse_environment: Option<String>,
    /// Langfuse release 字段
    pub langfuse_release: Option<String>,
    /// Langfuse version 字段
    pub langfuse_version: Option<String>,
    /// Langfuse public 开关（None = 不设置）
    pub langfuse_public: Option<bool>,
    /// Langfuse metadata（值均为字符串：标量保留，数组/table 转紧凑 JSON）
    pub langfuse_metadata: Option<HashMap<String, String>>,
    /// 是否在 Langfuse OTEL span 中记录 LLM generation input
    pub langfuse_capture_input: bool,
    /// 是否在 Langfuse OTEL span 中记录 LLM generation output
    pub langfuse_capture_output: bool,
    /// Langfuse capture 最大字符数（超出截断）
    pub langfuse_capture_max_chars: usize,
    /// 是否脱敏敏感字段（api_key/token/secret/password 等）
    pub langfuse_redact_secrets: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            soft_limit: 50,
            hard_limit: 200,
            doom_loop_threshold: 5,
            llm_retry_attempts: 3,
            llm_retry_base_delay_ms: 1000,
            bash_confirm_mode: true,
            file_max_size_bytes: 1048576,
            max_depth: 5,
            max_concurrent_subagents: 3,
            langfuse_enabled: false,
            langfuse_user_id: None,
            langfuse_tags: None,
            langfuse_environment: None,
            langfuse_release: None,
            langfuse_version: None,
            langfuse_public: None,
            langfuse_metadata: None,
            langfuse_capture_input: false,
            langfuse_capture_output: false,
            langfuse_capture_max_chars: 20_000,
            langfuse_redact_secrets: true,
        }
    }
}

impl From<&AgentConfig> for LangfuseSpanConfig {
    fn from(cfg: &AgentConfig) -> Self {
        Self {
            langfuse_enabled: cfg.langfuse_enabled,
            langfuse_session_id: None,
            langfuse_trace_name: Some("visp.agent.run".to_string()),
            langfuse_user_id: cfg.langfuse_user_id.clone(),
            langfuse_tags: cfg.langfuse_tags.clone(),
            langfuse_environment: Some(
                cfg.langfuse_environment
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
            langfuse_release: cfg.langfuse_release.clone(),
            langfuse_version: cfg.langfuse_version.clone(),
            langfuse_public: cfg.langfuse_public,
            langfuse_metadata: cfg.langfuse_metadata.clone(),
        }
    }
}

// ── Internal helper ──────────────────────────────────────────────────────────

pub(crate) struct ToolExecResult {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) result: ToolResult,
    pub(crate) duration_ms: Option<u64>,
}

/// Pending sub-agent spawn (for multi-agent mode)
#[allow(dead_code)]
pub(crate) struct PendingSpawn {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) subagent_type: String,
}

pub(crate) fn llm_error_to_code(err: &LlmError) -> (AgentErrorCode, String) {
    match err {
        LlmError::Network(msg) => (AgentErrorCode::LlmNetwork, msg.clone()),
        LlmError::RateLimit { retry_after_secs } => (
            AgentErrorCode::LlmRateLimit,
            format!("rate limited, retry after {retry_after_secs}s"),
        ),
        LlmError::Auth(msg) => (AgentErrorCode::LlmAuth, msg.clone()),
        LlmError::Api { status, message } => (
            AgentErrorCode::LlmApi,
            format!("status {status}: {message}"),
        ),
        LlmError::Stream(msg) => (AgentErrorCode::LlmStream, msg.clone()),
        LlmError::Cancelled => (AgentErrorCode::Cancelled, "agent cancelled".into()),
    }
}

/// 格式化工具参数为用户友好的显示文本
pub(crate) fn format_tool_args(args_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(serde_json::Value::Object(obj)) => {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        _ => args_json.to_string(),
    }
}

// ── [USER_QUERY] marker parsing ──────────────────────────────────────────────

pub(crate) struct UserQueryMarker {
    pub(crate) message: String,
    pub(crate) options: Vec<String>,
    pub(crate) allow_other: bool,
}

/// 从文本末尾检测 [USER_QUERY]...[/USER_QUERY] 标记。
///
/// 约定：一个 marker 只能包含一个问题。若 LLM 违反约束塞入多个问题
/// （选项之后再次出现非空非选项行），防御性地只取第一个问题并告警，
/// 避免多个问题的选项被拍平成一个混合选项列表（UI 只支持单选）。
pub(crate) fn parse_user_query_marker(text: &str) -> Option<UserQueryMarker> {
    let text = text.trim_end();

    // 查找结尾标记（可选：LLM 可能截断输出导致缺闭合标记，容错解析到文本末尾）
    let close_pos = text.rfind("[/USER_QUERY]");
    let before_close = &text[..close_pos.unwrap_or(text.len())];

    // 查找开头标记（必须存在）
    let open_pos = before_close.rfind("[USER_QUERY")?;

    // 提取开头标记内容 [USER_QUERY ...]
    let header_end = before_close[open_pos..].find(']')?;
    let header = &before_close[open_pos..=open_pos + header_end];

    // 解析 allow_other
    let allow_other = header.contains("allow_other=true");

    // 提取标记内内容（去除头部标记行）
    let body_start = open_pos + header_end + 1;
    let body = match close_pos {
        Some(cp) if cp >= body_start => &before_close[body_start..cp],
        // 无闭合标记：解析到文本末尾（LLM 可能截断）
        _ => &before_close[body_start..],
    };
    let body = body.trim();

    if body.is_empty() {
        return None;
    }

    // 解析：首行是 message，- 前缀行为 options
    let mut message = String::new();
    let mut options = Vec::new();
    let mut saw_options = false;
    let mut truncated = false;

    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(opt_text) = trimmed.strip_prefix("- ") {
            options.push(opt_text.to_string());
            saw_options = true;
        } else if trimmed.is_empty() {
            // 空行：跳过（可能是选项之间的分隔）
        } else if message.is_empty() {
            message = trimmed.to_string();
        } else if saw_options {
            // 选项之后出现非空非选项行 = 检测到第二个问题，截断只保留第一个
            truncated = true;
            break;
        }
        // 其它（message 与选项之间的说明文本）保持原有忽略行为
    }

    if truncated {
        tracing::warn!(
            body = %body,
            "parse_user_query_marker: [USER_QUERY] contains multiple questions; only the first \
             question is used. The agent should ask one question per [USER_QUERY] marker."
        );
    }

    Some(UserQueryMarker {
        message,
        options,
        allow_other,
    })
}

/// 从文本中剥离 [USER_QUERY]...[/USER_QUERY] 标记
pub(crate) fn strip_user_query_marker(text: &str) -> String {
    let text = text.trim_end();
    if let Some(close_pos) = text.rfind("[/USER_QUERY]")
        && let Some(open_pos) = text[..close_pos].rfind("[USER_QUERY")
    {
        let before = &text[..open_pos].trim_end();
        return before.to_string();
    }
    text.to_string()
}

/// 从 thinking blocks 中提取 thinking 文本内容。
/// thinking block 的 JSON 结构: {"type":"thinking","thinking":"...","signature":"..."}
pub(crate) fn extract_thinking_text(blocks: &[serde_json::Value]) -> Option<String> {
    blocks.first().and_then(|block| {
        block
            .get("thinking")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    })
}

// Re-export run_agent_loop from agent_loop module
pub use crate::agent_loop::run_agent_loop;

/// 清理历史中残留的 orphan tool_uses。
/// 如果一条 assistant 消息包含 tool_calls，但没有对应的 tool_result
/// 消息紧随其后，则清空 tool_calls。这发生在 Cancel 终止了 agent 循环，
/// 导致 tool_use 被发送给 Anthropic 但没来得及追加 tool_result，
/// 后续请求会报 400 错误。
///
/// 注意：支持部分 tool_result 的情况——如果 assistant 消息有 3 个 tool_calls
/// 但只有 2 个 tool_results 跟随，也会清空 tool_calls。
pub(crate) fn cleanup_orphan_tool_uses(history: &mut [Message]) {
    let len = history.len();

    // 第一遍：从后向前，清理 orphan tool_calls（assistant 消息有 tool_calls 但没有配对的 tool 结果）
    let mut i = len;
    while i > 0 {
        i -= 1;
        if history[i].role == Role::Assistant
            && let Some(ref calls) = history[i].tool_calls
        {
            let num_calls = calls.len();
            // 紧跟在 assistant 后面的 num_calls 条消息必须全是 Tool 角色，
            // 且各自的 tool_call_id 必须与 assistant 的 ToolCallRequest.id 逐一对应
            let all_have_results = (0..num_calls).all(|offset| {
                let idx = i + 1 + offset;
                idx < len
                    && history[idx].role == Role::Tool
                    && history[idx].tool_call_id.as_deref() == Some(&calls[offset].id)
            });

            if !all_have_results {
                // 清理 orphan tool_calls
                // 同时标记对应的 tool 结果为 skip_context（如果存在但 ID 不匹配）
                for offset in 0..num_calls {
                    let idx = i + 1 + offset;
                    if idx < len && history[idx].role == Role::Tool {
                        // Tool 消息存在但 ID 不匹配 → 标记为 skip_context
                        history[idx].skip_context = true;
                    }
                }
                history[i].tool_calls = None;
            }
            // 不 break！继续检查更早的 assistant 消息
        }
    }

    // 第二遍：从后向前，清理孤儿 Tool 消息（tool 消息的 tool_call_id 没有对应的 assistant tool_call）
    let mut i = len;
    while i > 0 {
        i -= 1;
        if history[i].role == Role::Tool
            && let Some(ref call_id) = history[i].tool_call_id
        {
            // 向前查找最近的 assistant 消息
            let mut found = false;
            let mut j = i;
            while j > 0 {
                j -= 1;
                if history[j].role == Role::Assistant
                    && let Some(ref calls) = history[j].tool_calls
                {
                    // 找到了 assistant，检查是否有匹配的 tool_call id
                    if calls.iter().any(|c| c.id == *call_id) {
                        found = true;
                    }
                    break; // 找到最近的 assistant 消息就停止（不管是否匹配）
                } else if history[j].role == Role::User {
                    break; // 遇到 user 消息停止
                }
            }
            if !found {
                history[i].skip_context = true;
            }
        }
    }
}

/// 根据注册的工具定义，生成按分类分组的动态工具指南 Markdown 文本
pub(crate) fn render_tool_guide(registry: &ToolRegistry) -> String {
    let defs = registry.definitions();
    if defs.is_empty() {
        return String::new();
    }

    /// 3 个高层次 Code Understanding 工具，放在独立分组中，且从 Analyze 分组中排除
    const CODE_UNDERSTANDING_TOOLS: &[&str] =
        &["codegraph_context", "codegraph_trace", "codegraph_impact"];

    use std::collections::{HashMap, HashSet};
    let cu_set: HashSet<&str> = HashSet::from_iter(CODE_UNDERSTANDING_TOOLS.iter().copied());
    let mut cu_descs: HashMap<&str, &str> = HashMap::new();
    let mut grouped: HashMap<&str, Vec<&str>> = HashMap::new();
    for def in &defs {
        // 将 Code Understanding 工具从常规 category 分组中排除
        if cu_set.contains(def.name.as_str()) {
            cu_descs.insert(def.name.as_str(), def.description.as_str());
            continue;
        }
        let cat = if def.category.is_empty() {
            "other"
        } else {
            def.category.as_str()
        };
        grouped.entry(cat).or_default().push(def.name.as_str());
    }

    let mut parts = vec![
        "\n\n## Available Tools".to_string(),
        "\n**IMPORTANT**: Prefer specialized tools over `bash` when possible: \
         use `read_file` to read files, `edit_file`/`write_file` to modify them, \
         `grep`/`glob` to search files. \
         Use `codegraph_context`/`codegraph_trace`/`codegraph_impact` for quick symbol \
         lookups and call-chain tracing. \
         When you need to discover code (find files, locate patterns, search \
         the codebase) or execute bounded implementation tasks, \
         delegate to the appropriate sub-agent (e.g. @explorer, @fixer, \
         @designer, @oracle, @librarian). \
         See the sub-agent list in the Task Delegation section for details. \
         Only use `bash` when no other tool fits \
         (e.g. running build commands, git operations, or multi-step shell scripts)."
            .to_string(),
    ];

    // Code Understanding 独立分组（在 category 分组之前）
    if !cu_descs.is_empty() {
        parts.push("\n## Code Understanding (lightweight lookups)".to_string());
        for name in CODE_UNDERSTANDING_TOOLS {
            if let Some(desc) = cu_descs.get(name) {
                parts.push(format!("  {name}  — {desc}"));
            }
        }
    }

    // 按固定顺序输出 category
    let categories = [
        ("Common (prefer these first)", "common"),
        ("Analyze", "analyze"),
        ("Network", "network"),
        ("External (MCP)", "mcp"),
    ];

    for (label, cat) in &categories {
        if let Some(tools) = grouped.remove(*cat)
            && !tools.is_empty()
        {
            parts.push(format!("\n**{label}**:\n  {}", tools.join(", ")));
        }
    }

    // 剩余的 category
    for (cat, tools) in grouped {
        if !tools.is_empty() {
            parts.push(format!("\n**{cat}**:\n  {}", tools.join(", ")));
        }
    }

    parts.join("\n")
}

/// 将当前 prompt（messages + tools）保存到 `.visp/last-prompt.json`，
/// 方便调试和检查实际发送给 LLM 的内容。
/// 取消注释上方 `dump_prompt_to_file(&ctx.working_dir, &messages, &tools);` 以启用。
/// 写入失败时静默忽略。
#[allow(dead_code)]
pub(crate) fn dump_prompt_to_file(
    working_dir: &std::path::Path,
    messages: &[crate::message::Message],
    tools: &[crate::message::ToolDefinition],
) {
    let dir = working_dir.join(".visp");
    if !dir.is_dir() {
        return;
    }
    let path = dir.join("last-prompt.json");
    let content = serde_json::json!({
        "messages": messages,
        "tools": tools,
    });
    if let Ok(json) = serde_json::to_string_pretty(&content) {
        let _ = std::fs::write(&path, json);
    }
}

// ----------------------  test ------------------------//
// ----------------------  test ------------------------//
// ----------------------  test ------------------------//

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
