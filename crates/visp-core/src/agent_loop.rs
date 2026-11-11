use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::ProviderMetadata;
use crate::agent::{
    AgentConfig, AgentEvent, AgentLoopContext, AgentMessage, Envelope, PermissionKind,
    ToolExecResult, ToolOutcome, UserQueryResult, cleanup_orphan_tool_uses, extract_thinking_text,
    format_tool_args, llm_error_to_code, parse_user_query_marker, render_tool_guide,
    strip_user_query_marker,
};
use crate::error::AgentErrorCode;
use crate::error::LlmError;
use crate::message::{
    Message, MessageType, Role, ToolCallRequest, ToolDefinition, estimate_message_tokens,
};
use crate::prompt::PromptBuilder;
use crate::provider::ChatEvent;
use crate::provider::LlmConfig;
use crate::provider::LlmProvider;

/// 工具审批弹窗的超时秒数。用户在此时间内未响应则视为拒绝，
/// 避免 agent loop 因审批弹窗无人响应而无限挂起。
const APPROVAL_TIMEOUT_SECS: u64 = 120;

/// 连续 thinking-only（有思考、无正文、无工具）轮次的容忍上限。
///
/// provider 在思考预算耗尽后通常 1–2 轮即产出正文，因此 3 轮足够宽松；
/// 超过则几乎必然是异常（thinking 被 redacted、上游空转、解析缺口），继续
/// `Continue` 只会把一次对话拖成几十轮、耗时上百秒的空转。达到此上限后
/// 明确失败，不再续跑。
const MAX_CONSECUTIVE_THINKING_ONLY: u32 = 3;

/// 最终回答（有正文、无工具调用）因 max_tokens 被截断时，自动续写的次数上限。
///
/// 截断的回答以前被当作正常结束（ACP `end_turn`），调用方无从得知结论不完整；
/// 退化输出（同一句重复到 max_tokens）也会被当成结论。现在先续写一次，
/// 仍被截断则明确报错。
const MAX_TRUNCATED_CONTINUATIONS: u32 = 1;

/// 续写时插入的提示（与 doom-loop 提示同样以 system 消息进入历史）。
///
/// 两种截断都会走到这里：正文写到一半；或工具调用参数写到一半（openai.rs 会丢弃
/// 不完整的调用并补一句 "[tool call truncated ...]"）。后者最常见于一次写入大文件。
const TRUNCATION_CONTINUE_NOTICE: &str = "Your previous reply was cut off by the max_tokens limit. \
If you were making a tool call, it was dropped and nothing was executed: issue it again, \
splitting large content into smaller steps (for example, write a big file in several parts). \
Otherwise continue exactly where the text stopped without repeating it, and keep the rest concise.";

use crate::rules::RuleEngine;
use crate::session::SessionManager;
use crate::session::SessionStatus;
use crate::tool::ToolContext;
use crate::tool::ToolResult;
use crate::tool_registry::ToolRegistry;
use crate::trace_context::generate_w3c_span_id;

use visp_observability::{LangfuseSpanConfig, init_span};

use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::Instrument;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Convert AgentEvent to AgentMessage for global_tx forwarding
fn event_to_msg(event: &AgentEvent) -> Option<AgentMessage> {
    match event {
        AgentEvent::TextDelta(s) => Some(AgentMessage::TextDelta(s.clone())),
        AgentEvent::ThinkingBlock(v) => Some(AgentMessage::ThinkingBlock(v.clone())),
        // 思考增量：不转发给 orchestrator（与既有思考事件一致，orchestrator
        // 本就不转发思考文本；显示域由 tx/daemon 直接消费）。
        AgentEvent::ThinkingDelta(_) => None,
        AgentEvent::UsageInfo {
            input_tokens,
            output_tokens,
            tool_calls,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cost,
        } => Some(AgentMessage::UsageInfo {
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            tool_calls: *tool_calls,
            cache_creation_input_tokens: *cache_creation_input_tokens,
            cache_read_input_tokens: *cache_read_input_tokens,
            cost: *cost,
        }),
        AgentEvent::UsageDelta {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } => Some(AgentMessage::UsageDelta {
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cache_creation_input_tokens: *cache_creation_input_tokens,
            cache_read_input_tokens: *cache_read_input_tokens,
        }),
        AgentEvent::StatusUpdate(s) => Some(AgentMessage::StatusUpdate(s.clone())),
        AgentEvent::Error { code, message } => Some(AgentMessage::Error {
            code: code.clone(),
            message: message.clone(),
        }),
        AgentEvent::ToolCallRequest {
            call_id,
            tool_name,
            arguments,
        } => Some(AgentMessage::ToolCallRequest {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        }),
        AgentEvent::ToolCallResult {
            call_id,
            tool_name,
            content,
            is_error,
            outcome: _,
        } => Some(AgentMessage::ToolCallResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            content: content.clone(),
            is_error: *is_error,
        }),
        AgentEvent::UserQuery { .. } => None, // 不转发：AgentMessage::UserQuery 生产不可达
        AgentEvent::PreToolUse { .. } => None, // 不产出：仅为 hook 事实，无对应 AgentMessage
        AgentEvent::ImageBlock { .. } | AgentEvent::ImageError { .. } => None,
        AgentEvent::Done => Some(AgentMessage::Done),
    }
}

/// Send event to tx and optionally forward to global_tx.
/// On tx send failure, finishes session and returns Err(()).
async fn send_event(
    tx: &mpsc::Sender<AgentEvent>,
    sm: &SessionManager,
    sid: &str,
    global_tx: &Option<mpsc::Sender<Envelope>>,
    session_id: &str,
    event: AgentEvent,
) -> Result<(), ()> {
    // Forward to global_tx (for orchestrator)
    if let Some(gtx) = global_tx
        && let Some(msg) = event_to_msg(&event)
    {
        let _ = gtx
            .send(Envelope {
                session_id: session_id.to_string(),
                message: msg,
                trace_context: None,
            })
            .await;
    }
    // Send to tx (for CLI)
    if tx.send(event).await.is_err() {
        let _ = sm.finish_loop(sid, SessionStatus::Error);
        return Err(());
    }
    Ok(())
}

/// Iteration context produced by setup_iteration
struct IterationContext {
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
}

/// a. Cancellation check  b. Limits check  c. Prompt + tools build
#[allow(clippy::too_many_arguments)]
async fn setup_iteration(
    iteration: u32,
    ctx: &mut AgentLoopContext,
    sm: &SessionManager,
    tool_registry: &ToolRegistry,
    rule_engine: &RuleEngine,
    cfg: &AgentConfig,
    sid: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<IterationContext, ()> {
    // a. Cancellation check
    if ctx.cancel_token.is_cancelled() {
        tracing::info!(
            target: "visp.agent.cancelled",
            session_id = %sid,
            iteration,
            "agent loop cancelled"
        );
        send_event(
            tx,
            sm,
            sid,
            &ctx.global_tx,
            &ctx.session_id,
            AgentEvent::Error {
                code: AgentErrorCode::Cancelled,
                message: "agent cancelled".into(),
            },
        )
        .await?;
        let _ = sm.finish_loop(sid, SessionStatus::Error);
        return Err(());
    }

    // b. Limits check
    // Use `>` so that hard_limit=N allows exactly N iterations.
    // (iteration starts at 1; with `>=`, hard_limit=1 would block before
    //  any LLM call, breaking agents like vision/painter with steps=1.)
    if iteration > cfg.hard_limit {
        tracing::warn!(
            target: "visp.agent.iteration_limit",
            session_id = %sid,
            iteration,
            hard_limit = cfg.hard_limit,
            "agent loop reached iteration limit"
        );
        send_event(
            tx,
            sm,
            sid,
            &ctx.global_tx,
            &ctx.session_id,
            AgentEvent::Error {
                code: AgentErrorCode::MaxIterations,
                message: format!("Agent loop reached hard limit ({})", cfg.hard_limit),
            },
        )
        .await?;
        let _ = sm.finish_loop(sid, SessionStatus::Error);
        return Err(());
    }

    // c. Build prompt
    let system_prompt_template = match sm.get_system_prompt(sid) {
        Ok(t) => t,
        Err(e) => {
            send_event(
                tx,
                sm,
                sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::Error {
                    code: AgentErrorCode::Internal,
                    message: format!("Failed to get session: {e}"),
                },
            )
            .await?;
            let _ = sm.finish_loop(sid, SessionStatus::Error);
            return Err(());
        }
    };
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();

    let tool_guide = render_tool_guide(tool_registry);
    let mut enriched_template = if tool_guide.is_empty() {
        system_prompt_template.clone()
    } else {
        format!("{}{}", system_prompt_template, tool_guide)
    };

    // Soft limit check
    if cfg.soft_limit > 0 && iteration >= cfg.soft_limit {
        enriched_template.push_str(
            "\n\n[System: You have reached the maximum number of iterations. \
         Please immediately complete all remaining work in your next response. \
         If all work is done, respond without making any tool calls.]",
        );
    }
    let mut messages = PromptBuilder::build(
        &enriched_template,
        &rule_engine.get_active_rules(),
        &ctx.history,
        &ctx.working_dir,
        &date_str,
        Some(ctx.config.max_context_tokens),
        ctx.config.max_tokens,
        ctx.context_trimmer.as_ref(),
    );

    cleanup_orphan_tool_uses(&mut messages);
    messages.retain(|m| !m.skip_context);

    let tools = tool_registry.definitions();

    let total_estimated: u32 = messages
        .iter()
        .map(crate::message::estimate_message_tokens)
        .sum();
    tracing::info!(
        session_id = %sid,
        messages = messages.len(),
        budget = ctx.config.max_context_tokens,
        estimated_tokens = total_estimated,
        max_output_tokens = ctx.config.max_tokens,
        "prompt built, calling LLM"
    );

    Ok(IterationContext { messages, tools })
}

/// d. LLM call with retry for RateLimit/Network errors
///
/// 纯逻辑：监听 cancel_token、按配置重试，错误透传给调用方。
/// 不发 AgentEvent::Error、不调 finish_loop——这些 side-effect 由调用方负责。
async fn call_llm_with_retry(
    provider: &dyn LlmProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    ctx: &AgentLoopContext,
    cfg: &AgentConfig,
    sid: &str,
) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
    let mut attempt = 0u32;

    // 注入 Langfuse trace 级字段到 LlmConfig，使 provider 能在 gen_ai.client.operation span 上记录
    let llm_config = LlmConfig {
        langfuse_enabled: cfg.langfuse_enabled,
        langfuse_session_id: Some(ctx.session_id.clone()),
        // opencode 网关要求 x-opencode-session 头（每会话稳定 ID）
        session_id: Some(ctx.session_id.clone()),
        langfuse_trace_name: Some("visp.agent.run".to_string()),
        langfuse_user_id: cfg.langfuse_user_id.clone(),
        langfuse_tags: cfg.langfuse_tags.clone(),
        langfuse_environment: cfg.langfuse_environment.clone(),
        langfuse_release: cfg.langfuse_release.clone(),
        langfuse_version: cfg.langfuse_version.clone(),
        langfuse_public: cfg.langfuse_public,
        langfuse_metadata: cfg.langfuse_metadata.clone(),
        ..ctx.config.clone()
    };

    loop {
        // 循环顶部检查 cancel：首次进入或 sleep 后立即返回
        if ctx.cancel_token.is_cancelled() {
            return Err(LlmError::Cancelled);
        }

        match provider
            .chat_stream(messages, tools, &llm_config, &ctx.cancel_token)
            .await
        {
            Ok(s) => break Ok(Box::pin(s)),
            Err(e) => {
                // Retry on RateLimit, Network, and 5xx server errors
                // (e.g. 503 "engine overloaded, please try again later").
                let is_retriable = matches!(&e, LlmError::RateLimit { .. } | LlmError::Network(_))
                    || matches!(
                        &e,
                        LlmError::Api { status, .. } if *status >= 500 && *status < 600
                    );

                if !is_retriable || attempt >= cfg.llm_retry_attempts {
                    let (code, msg) = llm_error_to_code(&e);
                    if matches!(code, AgentErrorCode::Cancelled) {
                        tracing::info!(
                            session_id = %sid,
                            attempts = attempt + 1,
                            "LLM call cancelled by user"
                        );
                    } else if is_retriable {
                        tracing::error!(
                            session_id = %sid,
                            error_code = ?code,
                            error_msg = %msg,
                            attempts = attempt + 1,
                            "LLM provider error after retries exhausted"
                        );
                    } else {
                        tracing::error!(
                            session_id = %sid,
                            error_code = ?code,
                            error_msg = %msg,
                            "LLM provider error"
                        );
                    }
                    return Err(e);
                }
                let delay = cfg.llm_retry_base_delay_ms * (1u64 << attempt);
                // gen_ai.client.retry：把重试决策发为可观测事件
                //（重试循环在 visp-core 统一拥有，覆盖全部 provider，含 anthropic/openai）
                let reason = match &e {
                    LlmError::RateLimit { .. } => "rate_limit",
                    LlmError::Network(_) => "network",
                    LlmError::Api { .. } => "server_error",
                    _ => "unknown",
                };
                tracing::warn!(
                    target: "gen_ai.client.retry",
                    session_id = %sid,
                    attempt,
                    reason,
                    delay_ms = delay,
                    "retrying LLM request"
                );
                // sleep 期间监听 cancel
                tokio::select! {
                    biased;
                    _ = ctx.cancel_token.cancelled() => {
                        return Err(LlmError::Cancelled);
                    }
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                }
                attempt += 1;
            }
        }
    }
}

/// Stream output produced by collect_stream_events
struct StreamOutput {
    text_buffer: String,
    thinking_blocks: Vec<serde_json::Value>,
    tool_calls: Vec<ToolCallRequest>,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: u32,
    cache_read_input_tokens: u32,
    /// provider 上报的本次调用成本（未提供为 None）
    cost: Option<f64>,
    provider_metadata: Option<ProviderMetadata>,
}

/// 将 provider 下发的累积思考**快照**换算为相对本轮上一帧的**真增量**。
///
/// - `snapshot` 以 `previous` 为前缀 → 返回差集尾部（正常累积增长）；
/// - 否则（新块 / 内容跳变 / 回退变短）→ 返回全文，调用方据此重置基线；
/// - 两者相等 → 返回空串，调用方据此抑制发射（如仅带签名的终帧）。
///
/// 基线是 `collect_stream_events` 的函数局部状态，天然以「一轮 LLM 响应」为界。
fn thinking_increment(previous: &str, snapshot: &str) -> String {
    match snapshot.strip_prefix(previous) {
        Some(tail) => tail.to_string(),
        None => snapshot.to_string(),
    }
}

/// e. Collect stream events (TextDelta, ThinkingBlock, ToolCall, UsageInfo)
/// Returns None on error (stream dropped, cancelled, or LLM error).
async fn collect_stream_events(
    stream: Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
    ctx: &mut AgentLoopContext,
    sid: &str,
    tx: &mpsc::Sender<AgentEvent>,
    sm: &SessionManager,
) -> Option<StreamOutput> {
    let mut text_buffer = String::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    let mut thinking_blocks: Vec<serde_json::Value> = Vec::new();
    // 本轮当前思考块的基线快照（相对增量的参照物），以一轮 LLM 响应为界。
    let mut thinking_baseline = String::new();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;
    let mut cache_creation_input_tokens: u32 = 0;
    let mut cache_read_input_tokens: u32 = 0;
    let mut cost: Option<f64> = None;
    let mut pending_metadata: Option<ProviderMetadata> = None;

    tracing::info!(
        session_id = %sid,
        "collect_stream_events: starting to collect LLM stream events"
    );

    let mut pin_stream = Box::pin(stream);
    loop {
        tokio::select! {
            biased;
            _ = ctx.cancel_token.cancelled() => {
                send_event(
                    tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                    AgentEvent::Error {
                        code: AgentErrorCode::Cancelled,
                        message: "agent cancelled".into(),
                    },
                ).await.ok()?;
                let _ = sm.finish_loop(sid, SessionStatus::Error);
                return None;
            }
            event = pin_stream.next() => {
                match event {
                    Some(Ok(ChatEvent::TextDelta(delta))) => {
                        text_buffer.push_str(&delta);
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::TextDelta(delta),
                        ).await.ok()?;
                    }
                    Some(Ok(ChatEvent::ImageBlock { path, mime_type, remote_url })) => {
                        // Flush accumulated text before image
                        if !text_buffer.is_empty() {
                            let text = std::mem::take(&mut text_buffer);
                            send_event(
                                tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                                AgentEvent::TextDelta(text),
                            ).await.ok()?;
                        }
                        // Send image event to CLI
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::ImageBlock {
                                path: path.clone(),
                                mime_type: mime_type.clone(),
                                remote_url: remote_url.clone(),
                            },
                        ).await.ok()?;
                        // Append marker to text_buffer for Message persistence
                        match &remote_url {
                            Some(url) => {
                                text_buffer.push_str(&format!("<image: | {}>", url));
                            }
                            None => {
                                text_buffer.push_str(&format!("<image: {}>", path));
                            }
                        }
                    }
                    Some(Ok(ChatEvent::ImageError { reason })) => {
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::ImageError { reason: reason.clone() },
                        ).await.ok()?;
                    }
                    Some(Ok(ChatEvent::ThinkingBlock(block))) => {
                        // 内部装配保持不变：保留本轮完整块（用于持久化与签名回传）。
                        thinking_blocks.clear();
                        thinking_blocks.push(block.clone());
                        // 事件层语义：快照 → 真增量。仅发射增量变体，流式路径
                        // 不得再发快照变体（否则消费方会再追加一次快照）。
                        let snapshot = block
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let delta = thinking_increment(&thinking_baseline, snapshot);
                        thinking_baseline.clear();
                        thinking_baseline.push_str(snapshot);
                        if !delta.is_empty() {
                            send_event(
                                tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                                AgentEvent::ThinkingDelta(delta),
                            ).await.ok()?;
                        }
                    }
                    Some(Ok(ChatEvent::UsageInfo { input_tokens: it, output_tokens: ot, cache_creation_input_tokens: ccit, cache_read_input_tokens: crit, cost: c, .. })) => {
                        input_tokens = it;
                        output_tokens = ot;
                        cache_creation_input_tokens = ccit;
                        cache_read_input_tokens = crit;
                        cost = c;
                    }
                    Some(Ok(ChatEvent::UsageDelta { input_tokens: dit, output_tokens: dot, cache_creation_input_tokens: dccit, cache_read_input_tokens: dcrit })) => {
                        // 逐 chunk 增量：立即转发用于实时速率显示，不累加进本地统计
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::UsageDelta {
                                input_tokens: dit,
                                output_tokens: dot,
                                cache_creation_input_tokens: dccit,
                                cache_read_input_tokens: dcrit,
                            },
                        ).await.ok()?;
                    }
                    Some(Ok(ChatEvent::ToolCall { id, name, arguments })) => {
                        tool_calls.push(ToolCallRequest { id, name, arguments });
                    }
                    Some(Ok(ChatEvent::OutputMetadata(meta))) => {
                        pending_metadata = Some(meta);
                    }
                    Some(Ok(ChatEvent::Done)) => {
                        tracing::info!(
                            session_id = %sid,
                            text_len = text_buffer.len(),
                            tool_call_count = tool_calls.len(),
                            thinking_block_count = thinking_blocks.len(),
                            has_metadata = pending_metadata.is_some(),
                            "collect_stream_events: received Done event"
                        );
                        break;
                    }
                    Some(Err(e)) => {
                        let (code, msg) = llm_error_to_code(&e);
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::Error { code, message: msg },
                        ).await.ok()?;
                        let _ = sm.finish_loop(sid, SessionStatus::Error);
                        return None;
                    }
                    None => {
                        let partial_len = text_buffer.len();
                        let tool_count = tool_calls.len();
                        let thinking_count = thinking_blocks.len();
                        tracing::warn!(
                            session_id = %sid,
                            partial_response_len = partial_len,
                            tool_calls_received = tool_count,
                            thinking_blocks = thinking_count,
                            has_metadata = pending_metadata.is_some(),
                            finish_reasons = ?pending_metadata.as_ref().map(|m| &m.finish_reasons),
                            "LLM stream ended without Done event - connection likely dropped"
                        );
                        send_event(
                            tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::Internal,
                                message: "LLM stream ended unexpectedly — the response may be incomplete, check API connection".into(),
                            },
                        ).await.ok()?;
                        let _ = sm.finish_loop(sid, SessionStatus::Error);
                        return None;
                    }
                }
            }
        }
    }

    Some(StreamOutput {
        text_buffer,
        tool_calls,
        thinking_blocks,
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        cost,
        provider_metadata: pending_metadata,
    })
}

/// Decision after handling stream result
enum StreamDecision {
    Done,
    UserQuery {
        response_rx: mpsc::Receiver<UserQueryResult>,
    },
    /// 有工具调用，需执行后继续循环。
    Continue,
    /// 仅思考、无正文/工具（provider 明确结束了一轮），按既有语义继续循环，
    /// 但由调用方对其连续次数计数以触发 F5 上限。
    ThinkingOnly,
    /// 有正文、无工具调用，但 provider 报告 length/max_tokens：回答被截断，
    /// 不能当作完成。正文已写入历史；由调用方决定续写或报错。
    Truncated,
}

/// provider 元数据是否声称本轮以工具调用结束。
///
/// provider/parser 一致性检查共用此判据（空响应分支、F4 分支），避免多处
/// 就地重写 `finish_reasons` 字符串判断而漂移。
fn provider_claims_tool_calls(meta: Option<&ProviderMetadata>) -> bool {
    meta.is_some_and(|m| {
        m.finish_reasons
            .iter()
            .any(|r| r == "tool_calls" || r == "function_call")
    })
}

/// Drain pending sub-agent spawns into error ToolExecResults.
/// Used on cancel/inbox-close paths to prevent orphan tool_uses.
/// f. Handle stream result: check [USER_QUERY] marker or return Done/Continue
/// On error, sends error events via send_event and returns Err(()).
async fn handle_stream_result(
    output: &StreamOutput,
    total_tool_calls: u32,
    ctx: &mut AgentLoopContext,
    sm: &SessionManager,
    sid: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<StreamDecision, ()> {
    let text_buffer = &output.text_buffer;
    let tool_calls = &output.tool_calls;
    let thinking_blocks = &output.thinking_blocks;
    let input_tokens = output.input_tokens;
    let output_tokens = output.output_tokens;
    let cache_creation_input_tokens = output.cache_creation_input_tokens;
    let cache_read_input_tokens = output.cache_read_input_tokens;
    let cost = output.cost;
    let provider_metadata = &output.provider_metadata;

    tracing::info!(
        session_id = %sid,
        text_len = text_buffer.len(),
        tool_call_count = tool_calls.len(),
        thinking_block_count = thinking_blocks.len(),
        has_metadata = provider_metadata.is_some(),
        finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
        output_tokens,
        "handle_stream_result: deciding what to do with stream output"
    );

    if tool_calls.is_empty() {
        // Check [USER_QUERY] marker
        if let Some(marker) = parse_user_query_marker(text_buffer) {
            let clean_text = strip_user_query_marker(text_buffer);
            let thinking_text = extract_thinking_text(thinking_blocks);

            // Save thinking as a separate message if present
            if let Some(ref thinking) = thinking_text {
                let mut thinking_msg = Message::thinking(thinking.clone());
                thinking_msg.estimated_tokens = estimate_message_tokens(&thinking_msg);
                ctx.history.push(thinking_msg.clone());
                if let Err(e) = sm.append_message(sid, thinking_msg) {
                    send_event(
                        tx,
                        sm,
                        sid,
                        &ctx.global_tx,
                        &ctx.session_id,
                        AgentEvent::Error {
                            code: AgentErrorCode::Internal,
                            message: format!("Failed to append thinking message: {e}"),
                        },
                    )
                    .await?;
                    let _ = sm.finish_loop(sid, SessionStatus::Error);
                    return Err(());
                }
            }

            // Save text message if present
            if !clean_text.is_empty() {
                let mut text_msg =
                    Message::assistant_with_metadata(clean_text.clone(), provider_metadata.clone());
                text_msg.extra_blocks = if thinking_blocks.is_empty() {
                    None
                } else {
                    Some(thinking_blocks.clone())
                };
                text_msg.actual_tokens_input = Some(input_tokens);
                text_msg.actual_tokens_output = Some(output_tokens);
                text_msg.actual_cache_read = Some(cache_read_input_tokens);
                text_msg.actual_cache_write = Some(cache_creation_input_tokens);
                ctx.history.push(text_msg.clone());
                if let Err(e) = sm.append_message(sid, text_msg) {
                    send_event(
                        tx,
                        sm,
                        sid,
                        &ctx.global_tx,
                        &ctx.session_id,
                        AgentEvent::Error {
                            code: AgentErrorCode::Internal,
                            message: format!("Failed to append assistant message: {e}"),
                        },
                    )
                    .await?;
                    let _ = sm.finish_loop(sid, SessionStatus::Error);
                    return Err(());
                }
            }

            // Send UserQuery event
            let (resp_tx, resp_rx) = mpsc::channel::<UserQueryResult>(1);
            send_event(
                tx,
                sm,
                sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::UserQuery {
                    query_id: format!("query-{}", ctx.history.len()),
                    message: marker.message.clone(),
                    options: marker.options.clone(),
                    allow_other: marker.allow_other,
                    kind: PermissionKind::Question,
                    respond: resp_tx,
                },
            )
            .await?;

            return Ok(StreamDecision::UserQuery {
                response_rx: resp_rx,
            });
        }

        // No [USER_QUERY] marker: done
        if text_buffer.is_empty() && tool_calls.is_empty() && thinking_blocks.is_empty() {
            // Provider/parser consistency: an empty response that nonetheless
            // claims a tool-call finish reason means parsing and the provider's
            // shape disagree. Fail loudly rather than silently complete Done.
            if provider_claims_tool_calls(provider_metadata.as_ref()) {
                tracing::error!(
                    session_id = %sid,
                    input_tokens,
                    output_tokens,
                    finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
                    "empty response but provider reported a tool-call finish reason; \
                     stream parsing and provider shape are inconsistent"
                );
                send_event(
                    tx,
                    sm,
                    sid,
                    &ctx.global_tx,
                    &ctx.session_id,
                    AgentEvent::Error {
                        code: AgentErrorCode::Internal,
                        message: "provider 报告 tool_calls 但未解析到任何工具调用（SSE 解析与 provider 形态不一致）"
                            .into(),
                    },
                )
                .await?;
                let _ = sm.finish_loop(sid, SessionStatus::Error);
                return Err(());
            }
            if output_tokens > 0 {
                tracing::error!(
                    session_id = %sid,
                    input_tokens,
                    output_tokens,
                    finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
                    has_metadata = provider_metadata.is_some(),
                    "LLM returned empty response after consuming output tokens"
                );
                send_event(
                    tx, sm, sid, &ctx.global_tx, &ctx.session_id,
                    AgentEvent::Error {
                        code: AgentErrorCode::Internal,
                        message: format!(
                            "LLM returned empty response after consuming {output_tokens} output tokens. \
                         If thinking mode is enabled, the thinking may have been redacted or \
                         the output budget exhausted. Try increasing budget_tokens or max_tokens."
                        ),
                    },
                )
                .await?;
                let _ = sm.finish_loop(sid, SessionStatus::Error);
                return Err(());
            }
            tracing::warn!(
                session_id = %sid,
                input_tokens,
                output_tokens,
                "LLM returned empty stream (no text, no tool calls, no thinking)"
            );
        } else if text_buffer.is_empty() {
            tracing::info!(
                session_id = %sid,
                tool_calls = tool_calls.len(),
                thinking_blocks = thinking_blocks.len(),
                "LLM returned response with no text (only tool_calls/thinking)"
            );
        }

        // If the LLM produced only thinking (no text, no tool calls), the
        // model ended its turn without generating an actual response. This can
        // happen when:
        //   - finish_reason is "length"/"max_tokens" (token budget exhausted
        //     during thinking)
        //   - finish_reason is "stop" (some providers like DeepSeek via OpenAI
        //     protocol return "stop" after exhausting the thinking budget)
        // In both cases, save the thinking to history and continue the loop so
        // the LLM can generate the actual response.
        let is_token_limit = provider_metadata
            .as_ref()
            .map(|m| {
                m.finish_reasons
                    .iter()
                    .any(|r| r == "length" || r == "max_tokens")
            })
            .unwrap_or(false);

        // Continue the loop when we have provider metadata (the model actually
        // responded and finished). Without metadata (e.g. redacted_thinking),
        // we treat the thinking-only response as complete.
        let has_metadata = provider_metadata.is_some();

        // Provider/parser consistency check (must run BEFORE the thinking-only
        // branch below). If the provider reports a tool-call finish reason but
        // the stream parser yielded no tool calls, the SSE parsing and the
        // provider's shape disagree. Falling through to the thinking-only branch
        // would silently Continue until the iteration limit — the observed
        // no-op spin where tool calls were requested but never executed. Fail
        // loudly with a diagnosable error instead.
        let claims_tool_calls = provider_claims_tool_calls(provider_metadata.as_ref());
        if claims_tool_calls && tool_calls.is_empty() {
            tracing::error!(
                session_id = %sid,
                finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
                thinking_block_count = thinking_blocks.len(),
                has_text = !text_buffer.is_empty(),
                output_tokens,
                "provider reported a tool-call finish reason but no tool calls were parsed; \
                 stream parsing and provider shape are inconsistent"
            );
            send_event(
                tx,
                sm,
                sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::Error {
                    code: AgentErrorCode::Internal,
                    message: "provider 报告 tool_calls 但未解析到任何工具调用（SSE 解析与 provider 形态不一致）"
                        .into(),
                },
            )
            .await?;
            let _ = sm.finish_loop(sid, SessionStatus::Error);
            return Err(());
        }

        if text_buffer.is_empty()
            && tool_calls.is_empty()
            && !thinking_blocks.is_empty()
            && has_metadata
        {
            tracing::info!(
                session_id = %sid,
                output_tokens,
                is_token_limit,
                finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
                "LLM produced thinking-only response (no text/tool calls), continuing loop"
            );
            // Save thinking to history before continuing
            let thinking_text = extract_thinking_text(thinking_blocks);
            if let Some(ref thinking) = thinking_text {
                let mut thinking_msg = Message::thinking(thinking.clone());
                // Preserve the raw thinking blocks in extra_blocks so they can be
                // passed back to the API (DeepSeek requires reasoning_content).
                thinking_msg.extra_blocks = Some(thinking_blocks.clone());
                thinking_msg.estimated_tokens = estimate_message_tokens(&thinking_msg);
                ctx.history.push(thinking_msg.clone());
                if let Err(e) = sm.append_message(sid, thinking_msg) {
                    send_event(
                        tx,
                        sm,
                        sid,
                        &ctx.global_tx,
                        &ctx.session_id,
                        AgentEvent::Error {
                            code: AgentErrorCode::Internal,
                            message: format!("Failed to append thinking message: {e}"),
                        },
                    )
                    .await?;
                    let _ = sm.finish_loop(sid, SessionStatus::Error);
                    return Err(());
                }
            }
            return Ok(StreamDecision::ThinkingOnly);
        }

        let thinking_text = extract_thinking_text(thinking_blocks);

        if let Some(ref thinking) = thinking_text {
            let mut thinking_msg = Message::thinking(thinking.clone());
            thinking_msg.estimated_tokens = estimate_message_tokens(&thinking_msg);
            ctx.history.push(thinking_msg.clone());
            if let Err(e) = sm.append_message(sid, thinking_msg) {
                send_event(
                    tx,
                    sm,
                    sid,
                    &ctx.global_tx,
                    &ctx.session_id,
                    AgentEvent::Error {
                        code: AgentErrorCode::Internal,
                        message: format!("Failed to append thinking message: {e}"),
                    },
                )
                .await?;
                let _ = sm.finish_loop(sid, SessionStatus::Error);
                return Err(());
            }
        }

        if !text_buffer.is_empty() {
            let mut text_msg =
                Message::assistant_with_metadata(text_buffer.clone(), provider_metadata.clone());
            text_msg.extra_blocks = if thinking_blocks.is_empty() {
                None
            } else {
                Some(thinking_blocks.clone())
            };
            text_msg.actual_tokens_input = Some(input_tokens);
            text_msg.actual_tokens_output = Some(output_tokens);
            text_msg.actual_cache_read = Some(cache_read_input_tokens);
            text_msg.actual_cache_write = Some(cache_creation_input_tokens);
            ctx.history.push(text_msg.clone());
            if let Err(e) = sm.append_message(sid, text_msg) {
                send_event(
                    tx,
                    sm,
                    sid,
                    &ctx.global_tx,
                    &ctx.session_id,
                    AgentEvent::Error {
                        code: AgentErrorCode::Internal,
                        message: format!("Failed to append assistant message: {e}"),
                    },
                )
                .await?;
                let _ = sm.finish_loop(sid, SessionStatus::Error);
                return Err(());
            }
        }

        if is_token_limit && !text_buffer.is_empty() {
            tracing::warn!(
                session_id = %sid,
                text_len = text_buffer.len(),
                output_tokens,
                finish_reasons = ?provider_metadata.as_ref().map(|m| &m.finish_reasons),
                "final answer truncated by max_tokens; not treating it as completed"
            );
            return Ok(StreamDecision::Truncated);
        }

        // Send usage info and Done
        tracing::info!(
            session_id = %sid,
            text_len = text_buffer.len(),
            thinking_block_count = thinking_blocks.len(),
            "handle_stream_result: sending Done (no text, no tool calls, no thinking-only recovery)"
        );
        send_event(
            tx,
            sm,
            sid,
            &ctx.global_tx,
            &ctx.session_id,
            AgentEvent::UsageInfo {
                input_tokens,
                output_tokens,
                tool_calls: total_tool_calls,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                cost,
            },
        )
        .await?;
        send_event(
            tx,
            sm,
            sid,
            &ctx.global_tx,
            &ctx.session_id,
            AgentEvent::Done,
        )
        .await?;
        let _ = sm.finish_loop(sid, SessionStatus::Completed);
        return Ok(StreamDecision::Done);
    }

    Ok(StreamDecision::Continue)
}

/// g+h: Doom loop detection, execute tools in parallel, collect results,
/// and append tool results to history.
/// Returns true if the agent loop should return (fatal error), false to continue.
#[allow(clippy::too_many_arguments)]
async fn execute_tool_calls(
    tool_calls: &[ToolCallRequest],
    text_buffer: String,
    thinking_blocks: Vec<serde_json::Value>,
    actual_tokens_input: u32,
    actual_tokens_output: u32,
    actual_cache_read: u32,
    actual_cache_write: u32,
    provider_metadata: Option<ProviderMetadata>,
    total_tool_calls: &mut u32,
    ctx: &mut AgentLoopContext,
    sm: &Arc<SessionManager>,
    tool_registry: &Arc<ToolRegistry>,
    sid: &str,
    tx: &mpsc::Sender<AgentEvent>,
    cfg: &AgentConfig,
    doom_loop_window: &mut Vec<Vec<(String, serde_json::Value)>>,
    doom_loop_warned: &mut bool,
    visp_trace_id: &str,
    iter_span_w3c_id: &str,
) -> bool {
    // Append assistant message with tool_calls
    *total_tool_calls += tool_calls.len() as u32;
    let mut assistant_msg = Message {
        role: Role::Assistant,
        kind: MessageType::ToolCall,
        content: text_buffer,
        tool_call_id: None,
        tool_calls: Some(tool_calls.to_vec()),
        skip_context: false,
        extra_blocks: if thinking_blocks.is_empty() {
            None
        } else {
            Some(thinking_blocks)
        },
        estimated_tokens: 0,
        actual_tokens_input: Some(actual_tokens_input),
        actual_tokens_output: Some(actual_tokens_output),
        actual_cache_read: Some(actual_cache_read),
        actual_cache_write: Some(actual_cache_write),
        actual_cost: None,
        provider_metadata: provider_metadata
            .map(|pm| serde_json::to_value(pm).expect("ProviderMetadata serialization")),
        tool_call_count: Some(tool_calls.len() as u32),
        tool_result_is_error: None,
        tool_result_duration_ms: None,
        created_at: None,
        images: Vec::new(),
    };
    assistant_msg.estimated_tokens = estimate_message_tokens(&assistant_msg);
    ctx.history.push(assistant_msg.clone());
    if let Err(e) = sm.append_message(sid, assistant_msg) {
        let _ = send_event(
            tx,
            sm,
            sid,
            &ctx.global_tx,
            &ctx.session_id,
            AgentEvent::Error {
                code: AgentErrorCode::Internal,
                message: format!("Failed to append assistant message: {e}"),
            },
        )
        .await;
        // Send failure is acceptable here (best-effort), but mark error
        let _ = sm.finish_loop(sid, SessionStatus::Error);
        return true; // fatal
    }

    // g. Doom loop detection
    if cfg.doom_loop_threshold > 0 {
        // Only *valid* tool calls participate in loop detection. Calls whose
        // arguments fail to parse (empty/truncated/invalid JSON — e.g. a
        // streaming interruption producing 0-byte arguments) are broken calls,
        // not repeated behavior: folding them all to Null made distinct broken
        // calls look identical and falsely tripped StuckInLoop. Skip a round
        // entirely when no valid call remains, so empty rounds don't compare
        // equal to each other.
        let round_sig: Vec<(String, serde_json::Value)> = tool_calls
            .iter()
            .filter_map(|tc| {
                serde_json::from_str(&tc.arguments)
                    .ok()
                    .map(|args| (tc.name.clone(), args))
            })
            .collect();
        if !round_sig.is_empty() {
            doom_loop_window.push(round_sig);
            let threshold = cfg.doom_loop_threshold as usize;
            if doom_loop_window.len() > threshold {
                doom_loop_window.remove(0);
            }
            if doom_loop_window.len() == threshold {
                let first = &(*doom_loop_window)[0];
                let all_same = doom_loop_window.iter().all(|sig| sig == first);
                if all_same {
                    if *doom_loop_warned {
                        let _ = send_event(
                            tx,
                            sm,
                            sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::StuckInLoop,
                                message: "Agent stuck in repeated tool call loop after warning"
                                    .into(),
                            },
                        )
                        .await;
                        let _ = sm.finish_loop(sid, SessionStatus::Error);
                        return true; // fatal
                    }
                    *doom_loop_warned = true;
                    doom_loop_window.clear();
                    let _ = send_event(
                        tx,
                        sm,
                        sid,
                        &ctx.global_tx,
                        &ctx.session_id,
                        AgentEvent::StatusUpdate(
                            "Agent appears stuck in a loop of repeated tool calls".into(),
                        ),
                    )
                    .await;
                    ctx.history.push(Message::system(
                        "You appear to be repeating the same tool calls. \
                         Please change your approach or summarize the current progress.",
                    ));
                }
            }
        }
    }

    // h. Execute tools in parallel
    let num_tools = tool_calls.len();
    let mut exec_tasks = Vec::with_capacity(num_tools);
    let tool_ids: Vec<String> = tool_calls.iter().map(|tc| tc.id.clone()).collect();
    let langfuse_enabled = cfg.langfuse_enabled;

    for (i, tc) in tool_calls.iter().enumerate() {
        // Regular tool execution
        let tx = tx.clone();
        let global_tx = ctx.global_tx.clone();
        let cancel = ctx.cancel_token.clone();
        let registry = tool_registry.clone();
        let session_id = sid.to_string();
        let working_dir = ctx.working_dir.clone();
        let tc = tc.clone();
        let sm = sm.clone();
        let permissions = ctx.permission_rules.clone();
        let sid2 = sid.to_string();
        let tool_visp_trace_id = visp_trace_id.to_string();
        let tool_iter_span_w3c_id = iter_span_w3c_id.to_string();

        // Pre-clone for tool span
        let tool_span_name = tc.name.clone();
        let tool_span_id = tc.id.clone();

        let tool_span = init_span!(
            &LangfuseSpanConfig::from(cfg),
            sid,
            "visp.agent.run",
            "visp.tool.execute",
            gen_ai.tool.name = %tool_span_name,
            gen_ai.tool.call.id = %tool_span_id,
            gen_ai.tool.type = "function",
            gen_ai.operation.name = "execute_tool",
        );

        tracing::info!(
            tool_name = %tc.name,
            call_id = %tc.id,
            "dispatching tool for execution"
        );

        exec_tasks.push(tokio::spawn(
            async move {
                // Helper: forward AgentMessage to global_tx
                macro_rules! forward_global {
                    ($msg:expr) => {
                        if let Some(ref gtx) = global_tx {
                            let _ = gtx.try_send(Envelope {
                                session_id: sid2.clone(),
                                message: $msg,
                                trace_context: None,
                            });
                        }
                    };
                }

                // Cancellation check
                if cancel.is_cancelled() {
                    let result = ToolExecResult {
                        index: i,
                        call_id: tc.id.clone(),
                        result: ToolResult::error("Cancelled"),
                        duration_ms: None,
                    };
                    let outcome = ToolOutcome::Cancelled;
                    forward_global!(AgentMessage::ToolCallResult {
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        content: "Cancelled".into(),
                        is_error: true,
                    });
                    let _ = tx
                        .send(AgentEvent::ToolCallResult {
                            call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: "Cancelled".into(),
                            outcome,
                            is_error: outcome.into_is_error(),
                        })
                        .await;
                    return result;
                }

                // Send ToolCallRequest
                forward_global!(AgentMessage::ToolCallRequest {
                    call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });
                let _ = tx
                    .send(AgentEvent::ToolCallRequest {
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .await;

                // Check if tool requires approval (with arguments)
                let args_value: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_default();
                let requires_approval = registry
                    .get(&tc.name)
                    .map(|t| t.requires_approval_for(&args_value))
                    .unwrap_or(false);

                let already_approved = sm.is_tool_approved(&session_id, &tc.name);

                if requires_approval && !already_approved {
                    let (resp_tx, mut resp_rx) = mpsc::channel::<UserQueryResult>(1);
                    let args_display = format_tool_args(&tc.arguments);
                    let _ = tx
                        .send(AgentEvent::UserQuery {
                            query_id: tc.id.clone(),
                            message: format!("Allow tool: {}({})?", tc.name, args_display),
                            options: Vec::new(),
                            allow_other: false,
                            kind: PermissionKind::Approval,
                            respond: resp_tx,
                        })
                        .await;

                    let result = tokio::select! {
                        biased;
                        // If the user hits Stop while a tool approval dialog
                        // is open, don't hang forever — treat as denied.
                        _ = cancel.cancelled() => UserQueryResult {
                            selected_index: -1,
                            text: String::new(),
                        },
                        // If the user doesn't respond to the approval dialog
                        // within the timeout, treat as denied instead of
                        // hanging the agent loop forever.
                        _ = tokio::time::sleep(std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS)) => UserQueryResult {
                            selected_index: -1,
                            text: String::new(),
                        },
                        r = resp_rx.recv() => r.unwrap_or_default(),
                    };
                    match result.selected_index {
                        0 => {}
                        2 => {
                            let _ = sm.add_approved_tool(&session_id, &tc.name);
                        }
                        _ => {
                            let result = ToolResult::error("User denied");
                            let outcome = ToolOutcome::Denied;
                            forward_global!(AgentMessage::ToolCallResult {
                                call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                content: result.content.clone(),
                                is_error: result.is_error,
                            });
                            let _ = tx
                                .send(AgentEvent::ToolCallResult {
                                    call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    content: result.content.clone(),
                                    outcome,
                                    is_error: outcome.into_is_error(),
                                })
                                .await;
                            return ToolExecResult {
                                index: i,
                                call_id: tc.id,
                                result,
                                duration_ms: None,
                            };
                        }
                    }
                }

                // Parse arguments and execute.
                // Empty (0-byte) arguments are normalized to `{}`: some
                // OpenAI-compatible endpoints emit `"arguments": ""` for
                // parameterless calls, and Anthropic sends no input_json_delta
                // when the model passes no input. Parameterless tools then run
                // normally; tools with required params fail via ToolResult and
                // the model retries with correct arguments.
                let raw_args: &str = if tc.arguments.trim().is_empty() {
                    "{}"
                } else {
                    &tc.arguments
                };
                let args = match serde_json::from_str(raw_args) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            tool = %tc.name,
                            args_len = tc.arguments.len(),
                            error = %e,
                            "tool call arguments truncated or malformed (likely max_output_tokens exceeded)"
                        );
                        // 内容超长被截断时必须拆分内容重发，不能原样重试。
                        let result = ToolResult::error(format!(
                            "[TRUNCATED] Tool call arguments incomplete ({} bytes, parse: {e}). \
                             The content exceeded max_output_tokens.\n\
                             To fix this, split the content into smaller parts:\n\
                             - Use multiple smaller write_file or edit_file calls\n\
                             - Or use edit_file to incrementally build the file\n\
                             - Do NOT retry the same large write_file call — it will fail again.",
                            tc.arguments.len(),
                        ));
                        let outcome = ToolOutcome::Truncated;
                        forward_global!(AgentMessage::ToolCallResult {
                            call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: result.content.clone(),
                            is_error: result.is_error,
                        });
                        let _ = tx
                            .send(AgentEvent::ToolCallResult {
                                call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                content: result.content.clone(),
                                outcome,
                                is_error: outcome.into_is_error(),
                            })
                            .await;
                        return ToolExecResult {
                            index: i,
                            call_id: tc.id,
                            result,
                            duration_ms: None,
                        };
                    }
                };

                // Real pre-execution point: no-approval, approved, Always Allow and
                // sub-agent runs all converge here. Denial, cancellation and
                // truncated/malformed arguments returned above, so they never emit.
                let _ = tx
                    .send(AgentEvent::PreToolUse {
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        requires_approval,
                    })
                    .await;

                let tool_ctx = ToolContext {
                    working_dir: working_dir.clone(),
                    session_id: Some(session_id),
                    permission_rules: permissions.clone(),
                    global_tx: global_tx.clone(),
                    visp_trace_id: Some(tool_visp_trace_id.clone()),
                    iter_span_w3c_id: Some(tool_iter_span_w3c_id.clone()),
                };

                let start = std::time::Instant::now();
                let result = registry
                    .execute(&tc.name, args, &tool_ctx)
                    .await
                    .unwrap_or_else(|| ToolResult::error("Tool not found in registry"));
                let elapsed_ms = start.elapsed().as_millis() as u64;

                // Record tool execution fields on the visp.tool.execute span
                tracing::Span::current().record("visp.tool.duration_ms", elapsed_ms as i64);
                tracing::Span::current().record("visp.tool.is_error", result.is_error);

                // Record tool arguments (skip edit/write to avoid logging file content)
                let tool_name = tc.name.as_str();
                if tool_name != "edit_file" && tool_name != "write_file" {
                    tracing::Span::current().record("visp.tool.arguments", &tc.arguments);
                }

                if langfuse_enabled {
                    let level = if result.is_error { "ERROR" } else { "DEFAULT" };
                    tracing::Span::current().record("level", level);
                    if result.is_error {
                        let summary = result.content.lines().next().unwrap_or("Tool error");
                        let status_msg = if summary.len() > 100 {
                            let mut s = summary.to_string();
                            s.truncate(97);
                            s.push_str("...");
                            s
                        } else {
                            summary.to_string()
                        };
                        tracing::Span::current().record("status_message", status_msg);
                    }
                }

                let outcome = if result.is_error {
                    ToolOutcome::Failure
                } else {
                    ToolOutcome::Success
                };
                forward_global!(AgentMessage::ToolCallResult {
                    call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                });
                let _ = tx
                    .send(AgentEvent::ToolCallResult {
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        content: result.content.clone(),
                        outcome,
                        is_error: outcome.into_is_error(),
                    })
                    .await;

            ToolExecResult {
                index: i,
                call_id: tc.id,
                result,
                duration_ms: Some(elapsed_ms),
            }
        }.instrument(tool_span)));
    }

    // Collect results
    let mut exec_tasks = Some(exec_tasks);
    let task_results = tokio::select! {
        biased;
        _ = ctx.cancel_token.cancelled() => {
            if let Some(tasks) = exec_tasks.take() {
                for h in &tasks { h.abort(); }
            }
            Vec::new()
        }
        results = futures::future::join_all(
            exec_tasks.take().unwrap()
        ) => {
            results.into_iter().enumerate().filter_map(|(idx, r)| match r {
                Ok(result) => Some(result),
                Err(e) if e.is_cancelled() => None,
                Err(e) => {
                    tracing::warn!("tool task {} failed: {e}", idx);
                    let call_id = tool_ids.get(idx).cloned().unwrap_or_default();
                    Some(ToolExecResult {
                        index: idx,
                        call_id,
                        result: ToolResult::error(format!("Tool execution panicked: {e}")),
                        duration_ms: None,
                    })
                }
            }).collect()
        }
    };

    tracing::info!(
        tool_count = task_results.len(),
        "all tool executions completed"
    );

    // Append tool results to history (in original order)    // Append tool results to history (in original order)
    let mut sorted_results: Vec<ToolExecResult> = task_results;
    sorted_results.sort_by_key(|r| r.index);

    for tr in sorted_results {
        let mut tool_msg =
            Message::tool_with_duration(tr.result.content.as_str(), &tr.call_id, tr.duration_ms);
        // 记录工具执行是否出错，持久化到 message 表的 tool_result_is_error 字段
        tool_msg.tool_result_is_error = Some(tr.result.is_error);
        ctx.history.push(tool_msg.clone());
        if let Err(e) = sm.append_message(sid, tool_msg) {
            let _ = send_event(
                tx,
                sm,
                sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::Error {
                    code: AgentErrorCode::Internal,
                    message: format!("Failed to append tool result: {e}"),
                },
            )
            .await;
            let _ = sm.finish_loop(sid, SessionStatus::Error);
            return true; // fatal
        }

        // Sub-agents (e.g. painter) may embed image references in their tool results
        // as `<image: | {url}>` (remote URL) or `<image: {path}>` (local file) markers.
        // Extract them and emit ImageBlock events so the main agent's CLI can display
        // the produced images. The markers are kept in the tool message content so the
        // LLM still sees them in its context.
        if !tr.result.is_error {
            let content = &tr.result.content;
            let mut search_from = 0;
            while let Some(rel_start) = content[search_from..].find("<image: ") {
                let marker_start = search_from + rel_start;
                let value_start = marker_start + "<image: ".len();
                if let Some(rel_end) = content[value_start..].find('>') {
                    let marker_end = value_start + rel_end;
                    let raw = content[value_start..marker_end].trim();

                    // Remote URL marker: `<image: | https://...>`
                    let event = if let Some(rest) = raw.strip_prefix('|') {
                        AgentEvent::ImageBlock {
                            path: String::new(),
                            mime_type: String::new(),
                            remote_url: Some(rest.trim().to_string()),
                        }
                    } else if !raw.is_empty() {
                        // Local file marker: `<image: /path/to/file.png>`
                        AgentEvent::ImageBlock {
                            path: raw.to_string(),
                            mime_type: String::new(),
                            remote_url: None,
                        }
                    } else {
                        search_from = marker_end + 1;
                        continue;
                    };
                    let _ = send_event(tx, sm, sid, &ctx.global_tx, &ctx.session_id, event).await;
                    search_from = marker_end + 1;
                } else {
                    break;
                }
            }
        }
    }

    false // continue loop
}

// ── Agent loop ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    provider: Arc<dyn LlmProvider>,
    tool_registry: Arc<ToolRegistry>,
    rule_engine: Arc<RuleEngine>,
    session_mgr: Arc<SessionManager>,
    mut ctx: AgentLoopContext,
    agent_config: &AgentConfig,
    user_message: Message,
    tx: mpsc::Sender<AgentEvent>,
) {
    // Manual span creation (replaces #[tracing::instrument]) – using
    // tracing::info_span! directly so the span is visible to test subscribers
    // created via set_default() on the same thread.
    let short_id = ctx.session_id[..ctx.session_id.len().min(8)].to_string();

    // Provider name from daemon.toml [[llm.models]] provider field (falls back to protocol)
    let provider_name = ctx
        .config
        .provider
        .as_deref()
        .unwrap_or("unknown")
        .to_string();

    let cfg = agent_config.clone();
    let lf_cfg = LangfuseSpanConfig::from(&cfg);
    let sid = ctx.session_id.clone();

    // Generate W3C span ID for this run span and record as field.
    // A downstream layer (SpanW3CIdInjector or ParentLinkLayer) reads this
    // via on_record and writes it into the span extension.
    let run_span_w3c_id = generate_w3c_span_id();

    let __run_span = init_span!(
        &lf_cfg,
        &sid,
        "visp.agent.run",
        "visp.agent.run",
        session.short_id = %short_id,
        gen_ai.provider.name = %provider_name,
        visp.agent.kind = %ctx.agent_kind,
        visp.agent.depth = ctx.depth,
        visp.agent.parent_session_id = %ctx.parent_session_id,
        visp.span.w3c_id = &run_span_w3c_id,
    );

    let __run_guard = __run_span.enter();

    let sm = session_mgr.clone();

    // Clone for use in panic handler after the async block
    let sid_panic = sid.clone();
    let sm_panic = sm.clone();
    // W2: Clone global_tx so the panic handler can forward AgentMessage::Error
    // to the orchestrator (which will then notify the parent agent via SubAgentError).
    let global_tx_panic = ctx.global_tx.clone();

    // Generate trace_id for W3C TraceContext propagation (Wave 1)
    let visp_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");

    // Record optional langfuse fields based on config
    if agent_config.langfuse_enabled && cfg.langfuse_capture_input {
        __run_span.record("langfuse.observation.type", "span");

        let json = serde_json::json!({"message": &user_message.content});
        let truncated = crate::sanitize::format_langfuse_input(
            &json,
            agent_config.langfuse_capture_max_chars,
            agent_config.langfuse_redact_secrets,
        );
        __run_span.record("langfuse.observation.input", &truncated);
    }

    // Clone root span handle for use inside async body (for output recording)
    let root_span = __run_span.clone();

    // Enter the span for the sync setup part, then wrap the async body with Instrument.
    drop(__run_guard);

    // Wrap entire body in catch_unwind for panic safety.
    // On panic, session is reset to Idle before re-raising.
    let result = AssertUnwindSafe(async move {
        if ctx.cancel_token.is_cancelled() {
            tracing::info!(
                target: "visp.agent.cancelled",
                session_id = %sid,
                "agent loop cancelled before start"
            );
            let _ = send_event(
                &tx,
                &sm,
                &sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::Error {
                    code: AgentErrorCode::Cancelled,
                    message: "agent cancelled".into(),
                },
            )
            .await;
            let _ = sm.finish_loop(&sid, SessionStatus::Error);
            return;
        }

        // Clean up orphan tool_uses from previous cancelled runs.
        cleanup_orphan_tool_uses(&mut ctx.history);

        // 1. Append user message to session store and local history
        if let Err(e) = sm.append_message(&sid, user_message.clone()) {
            let _ = send_event(
                &tx,
                &sm,
                &sid,
                &ctx.global_tx,
                &ctx.session_id,
                AgentEvent::Error {
                    code: AgentErrorCode::Internal,
                    message: format!("Failed to append user message: {e}"),
                },
            )
            .await;
            let _ = sm.finish_loop(&sid, SessionStatus::Error);
            return;
        }
        ctx.history.push(user_message);

        let mut total_tool_calls: u32 = 0;
        let mut iteration: u32 = 1;
        // F5：连续 thinking-only 轮次计数。进入 thinking-only 分支 +1，任何
        // 非 thinking-only 的轮次清零；超过 MAX_CONSECUTIVE_THINKING_ONLY 即中止。
        let mut consecutive_thinking_only: u32 = 0;
        // 连续被 max_tokens 截断的最终回答次数（见 MAX_TRUNCATED_CONTINUATIONS）。
        let mut consecutive_truncated: u32 = 0;
        let mut doom_loop_window: Vec<Vec<(String, serde_json::Value)>> = Vec::new();
        let mut doom_loop_warned = false;
        loop {
            tracing::info!(
                session_id = %sid,
                iteration,
                "=== agent loop iteration start ==="
            );
            // Create iteration span with W3C ID placeholder.
            let iter_span_w3c_id = generate_w3c_span_id();
            let iteration_span = init_span!(
                &lf_cfg,
                &sid,
                "visp.agent.run",
                "visp.agent.iteration",
                visp.span.w3c_id = &iter_span_w3c_id,
                iteration.count = iteration,
            );

            // a/b/c: Build prompt + boundary checks
            let ic = match setup_iteration(
                iteration,
                &mut ctx,
                &sm,
                &tool_registry,
                &rule_engine,
                &cfg,
                &sid,
                &tx,
            )
            .instrument(iteration_span.clone())
            .await
            {
                Ok(ic) => ic,
                Err(()) => {
                    // setup_iteration sends cancellaton / iteration_limit events internally
                    return;
                }
            };
            let messages = ic.messages;
            let tools = ic.tools;

            // d. Call LLM with retry
            let stream =
                match call_llm_with_retry(provider.as_ref(), &messages, &tools, &ctx, &cfg, &sid)
                    .instrument(iteration_span.clone())
                    .await
                {
                    Ok(s) => s,
                    Err(LlmError::Cancelled) => {
                        // retry 阶段被 cancel：还没拿到 stream，
                        // 必须在此显式发 Cancelled 事件 + finish_loop（collect_stream_events 不会被调用）
                        tracing::info!(
                            target: "visp.agent.cancelled",
                            session_id = %sid,
                            iteration,
                            "agent loop cancelled during LLM retry"
                        );
                        let _ = send_event(
                            &tx,
                            &sm,
                            &sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::Cancelled,
                                message: "agent cancelled".into(),
                            },
                        )
                        .await;
                        let _ = sm.finish_loop(&sid, SessionStatus::Error);
                        return;
                    }
                    Err(e) => {
                        let (code, msg) = llm_error_to_code(&e);
                        let _ = send_event(
                            &tx,
                            &sm,
                            &sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error { code, message: msg },
                        )
                        .await;
                        let _ = sm.finish_loop(&sid, SessionStatus::Error);
                        return;
                    }
                };

            // e. Collect stream events
            let output = match collect_stream_events(stream, &mut ctx, &sid, &tx, &sm)
                .instrument(iteration_span.clone())
                .await
            {
                Some(o) => o,
                None => {
                    tracing::warn!(
                        session_id = %sid,
                        iteration,
                        "run_agent_loop: collect_stream_events returned None, exiting loop"
                    );
                    return;
                }
            };

            // f. Handle stream result (check USER_QUERY marker or done)
            match handle_stream_result(&output, total_tool_calls, &mut ctx, &sm, &sid, &tx)
                .instrument(iteration_span.clone())
                .await
            {
                Ok(StreamDecision::Done) => {
                    // Record langfuse observation output (for trace preview)
                    if cfg.langfuse_enabled
                        && cfg.langfuse_capture_output
                        && !output.text_buffer.is_empty()
                    {
                        root_span.record("langfuse.observation.type", "span");

                        let json = serde_json::json!({"response": &output.text_buffer});
                        let truncated = crate::sanitize::format_langfuse_input(
                            &json,
                            cfg.langfuse_capture_max_chars,
                            cfg.langfuse_redact_secrets,
                        );
                        root_span.record("langfuse.observation.output", &truncated);
                    }

                    tracing::info!(
                        target: "visp.agent.completed",
                        session_id = %sid,
                        iterations = iteration,
                        total_tool_calls,
                        "agent loop completed"
                    );
                    tracing::info!(
                        session_id = %sid,
                        iteration,
                        total_tool_calls,
                        "run_agent_loop: StreamDecision::Done, agent loop completed normally"
                    );
                    return;
                }
                Ok(StreamDecision::UserQuery { mut response_rx }) => {
                    // Await the user's answer, but bail out immediately if the
                    // agent is cancelled (Stop pressed) — otherwise the loop
                    // hangs here forever.
                    let query_result = tokio::select! {
                        biased;
                        _ = ctx.cancel_token.cancelled() => {
                            tracing::info!(session_id = %sid, "agent cancelled while waiting for user query");
                            // 与 setup/retry/流式取消路径对齐：必须发收尾事件并落盘会话状态
                            let _ = send_event(
                                &tx,
                                &sm,
                                &sid,
                                &ctx.global_tx,
                                &ctx.session_id,
                                AgentEvent::Error {
                                    code: AgentErrorCode::Cancelled,
                                    message: "agent cancelled".into(),
                                },
                            )
                            .await;
                            let _ = sm.finish_loop(&sid, SessionStatus::Error);
                            return;
                        }
                        r = response_rx.recv() => r.unwrap_or_default(),
                    };

                    // Build user message from result
                    let marker = parse_user_query_marker(&output.text_buffer).unwrap();
                    let user_msg = if query_result.selected_index >= 0
                        && (query_result.selected_index as usize) < marker.options.len()
                    {
                        let option_text =
                            marker.options[query_result.selected_index as usize].clone();
                        Message::user(option_text)
                    } else {
                        Message::user(query_result.text)
                    };
                    ctx.history.push(user_msg.clone());
                    if let Err(e) = sm.append_message(&sid, user_msg) {
                        let _ = send_event(
                            &tx,
                            &sm,
                            &sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::Internal,
                                message: format!("Failed to append user message: {e}"),
                            },
                        )
                        .await;
                        let _ = sm.finish_loop(&sid, SessionStatus::Error);
                        return;
                    }
                    // 非 thinking-only 的轮次：清零连续计数。
                    consecutive_thinking_only = 0;
                    continue;
                }
                Ok(StreamDecision::Truncated) => {
                    consecutive_thinking_only = 0;
                    consecutive_truncated += 1;
                    if consecutive_truncated > MAX_TRUNCATED_CONTINUATIONS {
                        tracing::error!(
                            session_id = %sid,
                            consecutive_truncated,
                            iteration,
                            "final answer still truncated by max_tokens after continuation; aborting"
                        );
                        let _ = send_event(
                            &tx,
                            &sm,
                            &sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::LlmApi,
                                message: format!(
                                    "回答连续 {consecutive_truncated} 次在 max_tokens 处被截断，结论不完整（可调大 max_tokens，或检查模型是否在重复输出）"
                                ),
                            },
                        )
                        .await;
                        let _ = sm.finish_loop(&sid, SessionStatus::Error);
                        return;
                    }
                    tracing::info!(
                        session_id = %sid,
                        iteration,
                        consecutive_truncated,
                        "run_agent_loop: final answer truncated, asking the model to continue"
                    );
                    ctx.history.push(Message::system(TRUNCATION_CONTINUE_NOTICE));
                    // 无工具调用：下面的执行步骤会被跳过，直接进入下一轮。
                }
                Ok(StreamDecision::Continue) => {
                    // 有真实工具调用：清零连续 thinking-only 计数。
                    consecutive_thinking_only = 0;
                    consecutive_truncated = 0;
                    tracing::info!(
                        session_id = %sid,
                        iteration,
                        "run_agent_loop: StreamDecision::Continue, continuing to next iteration"
                    );
                }
                Ok(StreamDecision::ThinkingOnly) => {
                    consecutive_thinking_only += 1;
                    if consecutive_thinking_only > MAX_CONSECUTIVE_THINKING_ONLY {
                        tracing::error!(
                            session_id = %sid,
                            consecutive_thinking_only,
                            iteration,
                            finish_reasons = ?output
                                .provider_metadata
                                .as_ref()
                                .map(|m| &m.finish_reasons),
                            "consecutive thinking-only responses exceeded limit; \
                             aborting to avoid spinning"
                        );
                        let _ = send_event(
                            &tx,
                            &sm,
                            &sid,
                            &ctx.global_tx,
                            &ctx.session_id,
                            AgentEvent::Error {
                                code: AgentErrorCode::Internal,
                                message: format!(
                                    "连续 {consecutive_thinking_only} 轮仅思考无正文/工具，已中止以免空转"
                                ),
                            },
                        )
                        .await;
                        let _ = sm.finish_loop(&sid, SessionStatus::Error);
                        return;
                    }
                    tracing::info!(
                        session_id = %sid,
                        iteration,
                        consecutive_thinking_only,
                        "run_agent_loop: thinking-only response, continuing to next iteration"
                    );
                }
                Err(()) => {
                    tracing::warn!(
                        session_id = %sid,
                        iteration,
                        "run_agent_loop: handle_stream_result returned Err, exiting loop"
                    );
                    return;
                }
            }

            // g+h: Execute tool calls, collect results, append to history.
            // Only run when there are actual tool calls: empty tool_calls
            // (e.g. thinking-only responses) must not feed the doom-loop
            // window with identical empty signatures, which previously made
            // consecutive thinking-only turns trip "repeated tool call loop".
            if !output.tool_calls.is_empty()
                && execute_tool_calls(
                &output.tool_calls,
                output.text_buffer,
                output.thinking_blocks,
                output.input_tokens,
                output.output_tokens,
                output.cache_read_input_tokens,
                output.cache_creation_input_tokens,
                output.provider_metadata,
                &mut total_tool_calls,
                &mut ctx,
                &sm,
                &tool_registry,
                &sid,
                &tx,
                &cfg,
                &mut doom_loop_window,
                &mut doom_loop_warned,
                &visp_trace_id,
                &iter_span_w3c_id,
            )
            .instrument(iteration_span.clone())
            .await
            {
                return; // fatal error
            }

            // i. Increment iteration
            iteration += 1;
        }
    })
    .catch_unwind()
    .instrument(__run_span)
    .await;

    // If panicked, reset session status and notify orchestrator before re-raising.
    if let Err(panic) = result {
        let _ = sm_panic.finish_loop(&sid_panic, SessionStatus::Idle);

        // W2: Extract panic message and forward AgentMessage::Error to orchestrator.
        // This lets the orchestrator notify the parent agent via SubAgentError so
        // the parent's Phase 2 collection loop won't dead-wait on this sub-agent.
        let panic_msg = if let Some(s) = panic.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = panic.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };

        if let Some(global_tx) = global_tx_panic {
            // 使用 try_send 避免阻塞；失败仅记录日志（不影响 resume_unwind）
            let envelope = Envelope {
                session_id: sid_panic.clone(),
                message: AgentMessage::Error {
                    code: AgentErrorCode::Internal,
                    message: format!("agent loop panicked: {panic_msg}"),
                },
                trace_context: None,
            };
            match global_tx.try_send(envelope) {
                Ok(()) => {
                    tracing::error!(
                        session_id = %sid_panic,
                        panic = %panic_msg,
                        "agent loop panicked; error envelope forwarded to orchestrator"
                    );
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::error!(
                        session_id = %sid_panic,
                        panic = %panic_msg,
                        "agent loop panicked but global_tx is full; parent may dead-wait"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::error!(
                        session_id = %sid_panic,
                        panic = %panic_msg,
                        "agent loop panicked but global_tx is closed"
                    );
                }
            }
        } else {
            tracing::error!(
                session_id = %sid_panic,
                panic = %panic_msg,
                "agent loop panicked (no global_tx; root agent or test)"
            );
        }

        std::panic::resume_unwind(panic);
    }
}

#[cfg(test)]
#[path = "agent_loop_tests.rs"]
mod tests;
