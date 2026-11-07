//! Orchestrator — 多 Agent 运行时编排器
//!
//! 职责：
//! - 从 global_rx 接收 Envelope（Agent → Orchestrator）
//! - 从 grpc_rx 接收 ClientMessage（用户输入/查询响应）
//! - 发布 Agent 显示面事件到 `EventPublisher`（事件总线 → CLI）
//! - 管理子 Agent 的创建（spawn_sub_agent）、销毁（handle_done）、取消（cancel_agent）
//! - 管理 pending_queries 将用户响应路由到对应 agent

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing;
use tracing::Instrument;

use visp_core::agent::run_agent_loop;
use visp_core::agent::{
    AgentConfig, AgentEvent, AgentEventFrame, AgentKind, AgentLoopContext, AgentMessage, Envelope,
    OrchestratorMessage, UserQueryResult,
};
use visp_core::agent_definition::{AgentDefinition, merge_permissions};
use visp_core::agent_registry::AgentRegistry;
use visp_core::bus::{BusEvent, EventPublisher};
use visp_core::context::ContextTrimmer;
use visp_core::error::{AgentErrorCode, SessionError};
use visp_core::message::Message;
use visp_core::provider::LlmProvider;
use visp_core::rules::RuleEngine;
use visp_core::session::{SessionManager, SessionStatus, SubSessionParams};
use visp_core::tool::ToolType;
use visp_core::tool_registry::ToolRegistry;

use visp_config::DaemonConfig;
use visp_hooks::{
    HookContext, HookEvent, HookEventName, HookPayload, Origin, SessionSource, SessionStartPayload,
    StopFailurePayload, StopPayload, StopStatus, UserPromptSubmitPayload, VISP_HOOK_SCHEMA,
};

use visp_observability::LangfuseSpanConfig;

use crate::active_agent::{ActiveAgent, ActiveAgentRegistry};

/// 根据 allowed_sub_agents 筛选子 Agent 的工具列表
fn filter_tools_for_sub_agent(
    tool_registry: &ToolRegistry,
    allowed_sub_agents: &[String],
) -> Arc<ToolRegistry> {
    let filtered = ToolRegistry::new();
    for name in tool_registry.names() {
        if let Some(tool) = tool_registry.get(&name) {
            let should_include = match tool.tool_type() {
                ToolType::Agent => {
                    if allowed_sub_agents.is_empty() {
                        false
                    } else {
                        allowed_sub_agents.iter().any(|a| a == tool.name())
                    }
                }
                _ => true,
            };
            if should_include {
                // register 返回 Err 仅当名称重复——此处不会发生
                let _ = filtered.register(tool);
            }
        }
    }
    Arc::new(filtered)
}

/// 会话短标识（前 8 个字符；session id 为 UUID/ASCII，设计 §6.2 `short_id`）。
fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// 绑定时刻 `history` 是否为空 → 会话来源（设计 §6.2：空 → `startup`，非空 → `resume`）。
fn session_source(ctx: &AgentLoopContext) -> SessionSource {
    if ctx.history.is_empty() {
        SessionSource::Startup
    } else {
        SessionSource::Resume
    }
}

/// 从 AgentRegistry 动态构建子 agent 列表（用于注入 system prompt）
fn build_subagent_prompt(registry: &AgentRegistry) -> String {
    let subs: Vec<&AgentDefinition> = registry
        .list_subagents()
        .into_iter()
        .filter(|a| a.name != "default")
        .collect();

    if subs.is_empty() {
        return String::new();
    }

    let mut prompt = String::from("\n## Delegation Guidelines\n\n");
    prompt.push_str("Available sub-agents (delegate via @agent_name):\n");
    for def in subs {
        prompt.push_str(&format!("  - `{}` — {}\n", def.name, def.description));
    }
    prompt.push_str(
        "\nPrefer `explorer` for codebase search and `fixer` for code changes. \
         Delegate implementation tasks to `fixer` whenever the change is non-trivial.\n",
    );
    prompt
}

/// 取消信号
pub struct CancelSignal;

/// 排队等待执行的 subagent spawn 请求（保存 spawn_sub_agent 的全部参数）
struct QueuedSpawn {
    parent_session_id: String,
    call_id: String,
    subagent_type: String,
    description: String,
    prompt: String,
    task_id: Option<String>,
    trace_context: Option<visp_core::TraceContext>,
    response_tx: Option<tokio::sync::oneshot::Sender<String>>,
}

/// CLI → 服务器的消息
#[derive(Debug, Clone)]
pub enum ClientMessage {
    UserInput {
        session_id: String,
        text: String,
    },
    UserQueryResponse {
        query_id: String,
        selected_index: i32,
        text: String,
    },
    /// 取消正在运行的 agent
    Cancel {
        session_id: String,
    },
}

/// Orchestrator 错误
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("provider not found for key: {0}")]
    ProviderNotFound(String),
    #[error("max depth exceeded: {0}")]
    MaxDepthExceeded(u32),
}

/// 多 Agent 运行时编排器
pub struct Orchestrator {
    // ── 通道 ─────────────────────────────────────────────────
    cancel_rx: mpsc::Receiver<CancelSignal>,
    global_rx: mpsc::Receiver<Envelope>,
    global_tx: mpsc::Sender<Envelope>,
    grpc_rx: mpsc::Receiver<ClientMessage>,
    /// 显示面帧的唯一发布出口（事件总线，设计 §5 D3）。
    /// 发布抽象 [`EventPublisher`] 定义于 `visp-core`，daemon 以 `EventBus` 注入。
    bus: Arc<dyn EventPublisher>,

    // ── 状态 ─────────────────────────────────────────────────
    active_agents: ActiveAgentRegistry,
    pending_queries: HashMap<String, (String, mpsc::Sender<UserQueryResult>)>,
    /// 持有 sub-agent run_agent_loop 的 JoinHandle，便于诊断与未来扩展（如 abort）。
    /// key = sub session_id；handle 仅作引用持有，drop 不会 abort。
    sub_agent_handles: HashMap<String, JoinHandle<()>>,
    /// 待处理的 oneshot 响应通道，key 为 session_id
    pending_responses: HashMap<String, tokio::sync::oneshot::Sender<String>>,
    /// 因并发上限而排队等待的 spawn 请求（FIFO，subagent 完成释放空位后消费）
    queued_spawns: std::collections::VecDeque<QueuedSpawn>,
    /// 本 daemon 内已发射过 `SessionStart` 的 session 及其绑定时刻来源
    /// （设计 §6.2 裁定 #2：该 session 首次 `start_loop` 成功；同一 daemon 内第二个
    /// 新建会话仍会发）。会话级 `Stop`/`StopFailure` 复用同一 `source`。
    session_sources: HashMap<String, SessionSource>,
    /// 同回合最近一次 `UsageInfo`（input_tokens, output_tokens, tool_calls），
    /// 供会话级 `Stop` 载荷取值（设计 §6.2）。
    latest_usage: HashMap<String, (u64, u64, u64)>,

    // ── 共享依赖 ─────────────────────────────────────────────
    session_mgr: Arc<SessionManager>,
    agent_registry: Arc<ArcSwap<AgentRegistry>>,
    tool_registry: Arc<ToolRegistry>,
    rule_engine: Arc<RuleEngine>,
    agent_config: AgentConfig,
    context_trimmer: Arc<dyn ContextTrimmer + Send + Sync>,
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    /// Daemon 配置（llm.models 等），用于解析 agent 模型 key → LlmModelConfig
    daemon_config: Arc<DaemonConfig>,
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cancel_rx: mpsc::Receiver<CancelSignal>,
        global_rx: mpsc::Receiver<Envelope>,
        global_tx: mpsc::Sender<Envelope>,
        grpc_rx: mpsc::Receiver<ClientMessage>,
        session_mgr: Arc<SessionManager>,
        agent_registry: Arc<ArcSwap<AgentRegistry>>,
        tool_registry: Arc<ToolRegistry>,
        rule_engine: Arc<RuleEngine>,
        agent_config: AgentConfig,
        context_trimmer: Arc<dyn ContextTrimmer + Send + Sync>,
        daemon_config: Arc<DaemonConfig>,
        providers: HashMap<String, Arc<dyn LlmProvider>>,
        bus: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            cancel_rx,
            global_rx,
            global_tx,
            grpc_rx,
            bus,
            active_agents: ActiveAgentRegistry::new(),
            pending_queries: HashMap::new(),
            sub_agent_handles: HashMap::new(),
            pending_responses: HashMap::new(),
            queued_spawns: std::collections::VecDeque::new(),
            session_sources: HashMap::new(),
            latest_usage: HashMap::new(),
            session_mgr,
            agent_registry,
            tool_registry,
            rule_engine,
            agent_config,
            context_trimmer,
            daemon_config,
            providers,
        }
    }

    // ── 主循环 ───────────────────────────────────────────────

    /// 启动 Orchestrator 主循环
    /// 使用 biased select 保证取消信号优先处理
    pub async fn run(&mut self) {
        loop {
            tokio::select! {
                biased;
                Some(_signal) = self.cancel_rx.recv() => {
                    tracing::info!("orchestrator received cancel signal, shutting down");
                    for agent in self.active_agents.agents_cloned() {
                        self.cancel_agent(&agent.session_id);
                    }
                    break;
                }
                Some(envelope) = self.global_rx.recv() => {
                    self.handle_agent_message(envelope).await;
                }
                Some(msg) = self.grpc_rx.recv() => {
                    self.handle_client_message(msg).await;
                }
                else => break,
            }
        }
    }

    // ── 消息处理 ─────────────────────────────────────────────

    /// 处理来自 Agent 的消息（公开，供测试用）
    pub async fn handle_agent_message(&mut self, envelope: Envelope) {
        let session_id = envelope.session_id.clone();
        match envelope.message {
            AgentMessage::TextDelta(_) => {
                // TextDelta 已由 run_agent_loop 通过总线直接送达 CLI
                // 此处不重复转发
            }
            AgentMessage::ThinkingBlock(_) => {
                // 不在 V1 中转发
            }
            AgentMessage::UsageInfo {
                input_tokens,
                output_tokens,
                tool_calls,
                ..
            } => {
                // UsageInfo 已由 run_agent_loop 直接送达 CLI；此处仅记录同回合最近
                // 用量，供会话级 `Stop` 载荷取值（设计 §6.2）。
                self.latest_usage.insert(
                    session_id,
                    (input_tokens as u64, output_tokens as u64, tool_calls as u64),
                );
            }
            AgentMessage::UsageDelta { .. } => {
                // 逐 chunk 增量同样由 run_agent_loop 直送 CLI，此处不重复转发
            }
            AgentMessage::StatusUpdate(_) => {
                // StatusUpdate 已由 run_agent_loop 直接送达 CLI
            }
            AgentMessage::Error { code, message } => {
                // W2: 错误退出（含 panic 转发）走专属路径，向父 agent 投递 SubAgentError，
                // 而非 handle_done 默认的 SubAgentComplete 空内容路径。
                self.handle_agent_error(&session_id, code, message).await;
                // 错误退出同样释放并发空位 → 消费队列
                self.drain_queued_spawns().await;
            }
            AgentMessage::ToolCallRequest { .. } => {
                // ToolCallRequest 已由工具执行任务直接送达 CLI
            }
            AgentMessage::ToolCallResult { .. } => {
                // ToolCallResult 已由工具执行任务直接送达 CLI
            }
            AgentMessage::UserQuery {
                query_id,
                message,
                options,
                allow_other,
                respond,
            } => {
                self.pending_queries
                    .insert(query_id.clone(), (session_id.clone(), respond));
                // Look up agent context for the frame
                let agent_name = self
                    .active_agents
                    .get(&session_id)
                    .map(|a| a.agent_name.clone())
                    .unwrap_or_default();
                let parent_session_id = self
                    .active_agents
                    .get(&session_id)
                    .and_then(|a| a.parent_session_id.clone());
                let parent_session_name = parent_session_id
                    .as_ref()
                    .and_then(|pid| self.active_agents.get(pid).map(|a| a.agent_name.clone()));
                let frame = AgentEventFrame {
                    event: AgentEvent::UserQuery {
                        query_id,
                        message,
                        options,
                        allow_other,
                        // 帧由 AgentMessage::UserQuery 生产不可达（见 agent_loop `event_to_msg`），
                        // 此分支仅为类型完整保留；kind 取普通提问语义。
                        kind: visp_core::agent::PermissionKind::Question,
                        respond: mpsc::channel(1).0,
                    },
                    session_id: session_id.clone(),
                    agent_name,
                    parent_session_id,
                    parent_session_name,
                };
                self.bus.publish(BusEvent::Frame(frame));
            }
            AgentMessage::SpawnRequest {
                call_id,
                subagent_type,
                description,
                prompt,
                task_id,
                trace_context,
                response_tx,
            } => {
                // 优先使用 SpawnRequest 携带的 TraceContext（由父 agent 的
                // agent_loop 用父 trace_id 构造），确保子 agent 跨 mpsc
                // 边界继承父 trace_id。仅当未携带时才回退到从当前 span 提取
                // （OTel 不活跃时走 UUID fallback）。
                let trace_context =
                    trace_context.unwrap_or_else(crate::observability::extract_trace_context);
                self.spawn_sub_agent(
                    &envelope.session_id,
                    &call_id,
                    &subagent_type,
                    &description,
                    &prompt,
                    task_id.as_deref(),
                    Some(trace_context),
                    response_tx,
                )
                .await;
            }
            AgentMessage::Done => {
                self.handle_done(&session_id).await;
                // subagent 完成释放并发空位 → 从队列取出下一个等待的 spawn 请求执行
                self.drain_queued_spawns().await;
            }
        }
    }

    /// 处理来自 CLI 的消息（公开，供测试用）
    pub async fn handle_client_message(&mut self, msg: ClientMessage) {
        match msg {
            ClientMessage::UserInput { session_id, text } => {
                match self.session_mgr.get(&session_id) {
                    Ok(session) if session.parent_id.is_none() => {
                        // 恢复场景主 session 可能是 Completed/Error，重置为 Idle 再启动。
                        // 仅终态（Completed/Error）→ Idle；`Running` 不再重置——由 `start_loop`
                        // 的状态闸以 `SessionBusy` 拒绝，避免覆盖在途循环。
                        if matches!(
                            session.status,
                            SessionStatus::Completed | SessionStatus::Error
                        ) {
                            let _ = self
                                .session_mgr
                                .finish_loop(&session_id, SessionStatus::Idle);
                        }
                        self.start_main_agent(&session_id, &text).await;
                    }
                    _ => {}
                }
            }
            ClientMessage::UserQueryResponse {
                query_id,
                selected_index,
                text,
            } => {
                if let Some((_session_id, respond)) = self.pending_queries.remove(&query_id) {
                    let _ = respond
                        .send(UserQueryResult {
                            selected_index,
                            text,
                        })
                        .await;
                }
            }
            ClientMessage::Cancel { session_id } => {
                tracing::info!(%session_id, "cancelling agent by user request");
                self.cancel_agent(&session_id);
            }
        }
    }

    // ── Agent 生命周期 ───────────────────────────────────────

    /// 启动主 Agent（根，无 parent）
    async fn start_main_agent(&mut self, session_id: &str, user_message: &str) {
        let (agent_name, is_main) = match self.session_mgr.get(session_id) {
            Ok(s) => (s.agent_name.clone(), s.parent_id.is_none()),
            Err(e) => {
                tracing::error!(session_id, error = %e, "failed to get session");
                return;
            }
        };

        // Extract images early to determine routing
        let (clean_text, images) = Message::extract_images(user_message);
        let has_images = !images.is_empty();

        // 流程入口取一次 registry 快照并全程复用：Vision 路由查表与
        // subagent 列表渲染必须基于同一版本，避免中途 reload 替换造成撕裂。
        let registry = self.agent_registry.load_full();

        // If images are present, route to vision agent to avoid non-multimodal LLM errors
        let agent_def = if has_images {
            match registry.get("vision") {
                Some(vision_def) => {
                    tracing::info!(session_id, "images detected, routing to vision agent");
                    vision_def.clone()
                }
                None => {
                    tracing::warn!(
                        session_id,
                        "images detected but vision agent not found, falling back to main agent"
                    );
                    match registry.get(&agent_name) {
                        Some(a) => a.clone(),
                        None => {
                            tracing::error!(agent_name, "agent definition not found");
                            return;
                        }
                    }
                }
            }
        } else {
            match registry.get(&agent_name) {
                Some(a) => a.clone(),
                None => {
                    tracing::error!(agent_name, "agent definition not found");
                    return;
                }
            }
        };

        // Resolve provider - when images are present, use the vision agent's
        // model (multimodal) to avoid non-multimodal LLM errors.
        // For normal requests, session.config is the single source of truth;
        // user's /model switch won't be overridden by agent definition.
        let provider = match self
            .resolve_provider(if has_images { Some(&agent_def) } else { None }, session_id)
        {
            Some(p) => p,
            None => {
                tracing::error!(agent_name, "no provider available for main agent");
                return;
            }
        };

        // Permission rules (root agent: use agent default)
        let permissions = merge_permissions(&[], &[], &agent_def.permission);

        // Create loop context. 状态闸：非 Idle 会以 `SessionBusy` 失败。
        let mut ctx = match self.session_mgr.start_loop(
            session_id,
            &self.context_trimmer,
            Some(self.global_tx.clone()),
            Some(Arc::new(permissions)),
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                // 会话忙：不再静默丢弃，向客户端发布「会话忙」错误帧。
                if matches!(e, SessionError::SessionBusy { .. }) {
                    tracing::warn!(session_id, "start_loop rejected: session busy");
                    self.publish_session_busy(session_id, &agent_name);
                } else {
                    tracing::error!(session_id, error = %e, "start_loop_v2 failed");
                }
                return;
            }
        };

        // ── start_loop 成功后的准备工作（失败路径不再残留 / 累积）──────────────
        // Append agent-specific system prompt (from .visp/agents/*.md)
        if !agent_def.system_prompt.is_empty()
            && let Err(e) = self
                .session_mgr
                .append_system_prompt_template(session_id, &agent_def.system_prompt)
        {
            tracing::warn!(
                session_id,
                error = %e,
                "failed to append agent system prompt"
            );
        }

        // Append dynamic sub-agent delegation guidelines (skip for vision agent)
        if !has_images {
            let subagent_prompt = build_subagent_prompt(&registry);
            if !subagent_prompt.is_empty()
                && let Err(e) = self
                    .session_mgr
                    .append_system_prompt_template(session_id, &subagent_prompt)
            {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "failed to append subagent list to system prompt"
                );
            }
        }

        // Register active agent（start_loop 成功后才注册，避免忙失败残留 registration）
        let (inbox_tx, _inbox_rx) = mpsc::channel(64);
        self.active_agents.register(ActiveAgent {
            session_id: session_id.to_string(),
            parent_session_id: None,
            agent_name: agent_def.name.clone(),
            cancel_token: Default::default(),
            inbox: inbox_tx,
            pending_call_id: None,
            started_at: std::time::Instant::now(),
        });

        // When images are present, we route to the vision agent's provider
        // (multimodal model).  However, ctx.config still holds the session's
        // model (e.g. a text-only model).  The provider's chat_stream uses
        // config.model to build the API request, so we must override ctx.config
        // with the vision agent's model info to avoid sending images to a
        // text-only model.
        if has_images
            && let Some(model_key) = agent_def.model.as_deref()
            && let Some(model_cfg) = visp_config::resolve_model(model_key, &self.daemon_config)
        {
            visp_config::apply_model_override(&mut ctx.config, &model_cfg);
            ctx.config.model_key = Some(model_key.to_string());
            tracing::info!(
                session_id,
                model = %ctx.config.model,
                "applied vision agent model override to loop context"
            );
        }

        // Strip images from historical messages when the current turn has no
        // images.  This prevents stale image data (from a previous vision-agent
        // turn) being sent to a non-multimodal model on subsequent turns.
        if !has_images {
            for m in &mut ctx.history {
                m.images.clear();
            }
        }

        // hook 域（设计 §6.2 / §6.4）：start_loop 已成功，主会话在上述模型覆盖
        // 之后发射生命周期事件。
        //
        // `SessionStart` 仅在「该 session 本次 daemon 内首次 start_loop 成功」时发射：
        // per-session 标记集合保证同会话第二回合不再发，但同 daemon 内第二个新建
        // 会话仍会发；`is_main` 过滤（`parent_id.is_none()`）排除子 agent。
        if is_main
            && self
                .session_sources
                .insert(session_id.to_string(), session_source(&ctx))
                .is_none()
        {
            self.publish_session_start(session_id, &ctx, &agent_name);
        }
        // `UserPromptSubmit`：主 agent 受理 UserInput 且 start_loop 成功时发一次。
        self.publish_user_prompt_submit(session_id, &ctx, user_message);

        let mut msg = Message::user(&clean_text);
        msg.images = images;

        // Create forwarding task: agent_tx → bus with session context
        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(64);
        let bus = self.bus.clone();
        let sid = session_id.to_string();
        let agent_name = agent_name.clone();
        tokio::spawn(async move {
            while let Some(event) = agent_rx.recv().await {
                let frame = AgentEventFrame {
                    event,
                    session_id: sid.clone(),
                    agent_name: agent_name.clone(),
                    parent_session_id: None,
                    parent_session_name: None,
                };
                bus.publish(BusEvent::Frame(frame));
            }
        });

        let provider = provider.clone();
        let tool_registry = self.tool_registry.clone();
        let rule_engine = self.rule_engine.clone();
        let session_mgr = self.session_mgr.clone();
        let mut config = self.agent_config.clone();

        // Apply agent-level steps override as hard_limit
        if let Some(steps) = agent_def.steps {
            config.hard_limit = steps;
            if config.soft_limit == 0 || config.soft_limit >= steps {
                config.soft_limit = steps.saturating_mul(4) / 5;
            }
        }

        tokio::spawn(async move {
            run_agent_loop(
                provider,
                tool_registry,
                rule_engine,
                session_mgr,
                ctx,
                &config,
                msg,
                agent_tx,
            )
            .await;
        });
    }

    /// 启动子 Agent（响应 SpawnRequest）
    #[allow(clippy::too_many_arguments)]
    async fn spawn_sub_agent(
        &mut self,
        parent_session_id: &str,
        call_id: &str,
        subagent_type: &str,
        description: &str,
        prompt: &str,
        _task_id: Option<&str>,
        trace_context: Option<visp_core::TraceContext>,
        response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    ) {
        // 流程入口取一次 registry 快照：subagent 定义查表与父 agent 权限继承
        // 共享同一版本，避免中途 reload 替换造成两处不一致。
        let registry = self.agent_registry.load_full();

        // 1. Depth check
        let depth = self.active_agents.compute_depth(parent_session_id);
        if depth >= self.agent_config.max_depth {
            tracing::warn!(
                parent_session_id,
                depth,
                max = self.agent_config.max_depth,
                "max depth exceeded"
            );
            self.send_sub_agent_error(parent_session_id, call_id, "Max depth exceeded")
                .await;
            return;
        }

        // 1.5. 并发限制：达到上限 → 入队等待（父 agent 继续阻塞在 response_rx，
        //      待有 subagent 完成释放空位后由 drain_queued_spawns 消费执行）
        if self.active_subagent_count() >= self.concurrency_limit() {
            tracing::info!(
                count = self.active_subagent_count(),
                max = self.concurrency_limit(),
                "subagent concurrency limit reached, queuing spawn"
            );
            self.queued_spawns.push_back(QueuedSpawn {
                parent_session_id: parent_session_id.to_string(),
                call_id: call_id.to_string(),
                subagent_type: subagent_type.to_string(),
                description: description.to_string(),
                prompt: prompt.to_string(),
                task_id: _task_id.map(str::to_string),
                trace_context,
                response_tx,
            });
            return;
        }

        // 2. Look up agent definition
        let agent_def = match registry.get(subagent_type) {
            Some(a) => a.clone(),
            None => {
                tracing::error!(subagent_type, "subagent definition not found");
                self.send_sub_agent_error(
                    parent_session_id,
                    call_id,
                    &format!("Unknown subagent type: {subagent_type}"),
                )
                .await;
                return;
            }
        };

        // 3. Get parent session for permission inheritance, project_path, config
        let parent_session = match self.session_mgr.get(parent_session_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(parent_session_id, error = %e, "parent session not found");
                return;
            }
        };

        // 4. Merge permissions: parent session deny → parent agent deny → subagent rules
        let parent_agent_def = registry.get(&parent_session.agent_name);
        let parent_agent_permission = parent_agent_def
            .map(|a| a.permission.as_slice())
            .unwrap_or(&[]);
        let merged_rules = merge_permissions(
            &parent_session.permission,
            parent_agent_permission,
            &agent_def.permission,
        );

        // 5. Generate session ID — pure UUID, parent/agent stored in session table
        let sub_session_id = uuid::Uuid::new_v4().to_string();

        // 6. Create sub session
        let sub_session = match self.session_mgr.create_sub(SubSessionParams {
            parent_id: Some(parent_session_id.to_string()),
            agent_name: subagent_type.to_string(),
            permission: merged_rules.clone(),
            session_id: Some(sub_session_id.clone()),
            project_path: parent_session.project_path.clone(),
            config: parent_session.config.clone(),
            approved_tools: parent_session.approved_tools.clone(),
        }) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "create_sub failed");
                self.send_sub_agent_error(
                    parent_session_id,
                    call_id,
                    "Failed to create sub session",
                )
                .await;
                return;
            }
        };
        let sub_session_id = sub_session.id.clone();

        // 6.5. Apply agent-level model/temperature overrides
        //
        // The sub-session was created with `parent_session.config.clone()`.
        // If the agent definition specifies a `model` key, we resolve it to
        // the actual API model string and update the session's LlmConfig.
        // Similarly, `temperature` from the agent definition overrides the
        // inherited value.
        {
            let mut sub_config = sub_session.config.clone();

            // Resolve model info from agent_def.model key
            if let Some(model_key) = agent_def.model.as_deref()
                && let Some(model_cfg) = visp_config::resolve_model(model_key, &self.daemon_config)
            {
                visp_config::apply_model_override(&mut sub_config, &model_cfg);
                sub_config.model_key = Some(model_key.to_string());
                tracing::debug!(
                    sub_session_id,
                    agent = %subagent_type,
                    model = %sub_config.model,
                    "applied agent model override"
                );
            }

            // Agent-level temperature override takes precedence over model default
            if let Some(temp) = agent_def.temperature {
                sub_config.temperature = temp as f64;
            }

            // Persist the updated config to the session
            if let Err(e) = self.session_mgr.update_config(&sub_session_id, sub_config) {
                tracing::warn!(
                    sub_session_id,
                    error = %e,
                    "failed to apply agent config overrides"
                );
            }
        }

        // Append agent-specific system prompt (from .visp/agents/*.md)
        if !agent_def.system_prompt.is_empty()
            && let Err(e) = self
                .session_mgr
                .append_system_prompt_template(&sub_session_id, &agent_def.system_prompt)
        {
            tracing::warn!(
                sub_session_id,
                error = %e,
                "failed to append agent system prompt"
            );
        }

        // 7. Create inbox + register active agent
        let (inbox_tx, _inbox_rx) = mpsc::channel(64);
        self.active_agents.register(ActiveAgent {
            session_id: sub_session_id.clone(),
            parent_session_id: Some(parent_session_id.to_string()),
            agent_name: subagent_type.to_string(),
            cancel_token: Default::default(),
            inbox: inbox_tx,
            pending_call_id: Some(call_id.to_string()),
            started_at: std::time::Instant::now(),
        });

        // 8. Start loop context (consumes inbox_rx)
        let mut ctx = match self.session_mgr.start_loop(
            &sub_session_id,
            &self.context_trimmer,
            Some(self.global_tx.clone()),
            Some(Arc::new(merged_rules)),
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                tracing::error!(session_id = sub_session_id, error = %e, "start_loop failed for sub agent");
                self.active_agents.remove(&sub_session_id);
                self.send_sub_agent_error(
                    parent_session_id,
                    call_id,
                    "Failed to start sub agent loop",
                )
                .await;
                return;
            }
        };
        // Set agent kind and depth for observability
        ctx.agent_kind = AgentKind::Sub;
        ctx.depth = depth;

        // 9. Resolve provider
        let provider = match self.resolve_provider(Some(&agent_def), &sub_session_id) {
            Some(p) => p,
            None => {
                tracing::error!(subagent_type, "no provider available");
                self.active_agents.remove(&sub_session_id);
                self.send_sub_agent_error(parent_session_id, call_id, "No provider available")
                    .await;
                return;
            }
        };

        // 10. Send initial user message with the task prompt (fallback to description).
        //     `prompt` is the self-contained task written by the parent agent for the
        //     sub-agent; `description` is only a short label, used when prompt is empty.
        let task_msg = if prompt.is_empty() {
            description
        } else {
            prompt
        };
        let mut msg = Message::user(task_msg);

        // 10a. If the task prompt itself carries `<image: path>` markers, parse
        //      them so the sub-agent receives the actual image data (vision
        //      sub-agents rely on `msg.images` to "see" the image).  The marker
        //      is stripped from the text and the encoded image is attached.
        let (clean_text, prompt_images) = Message::extract_images(&msg.content);
        if !prompt_images.is_empty() {
            msg.content = clean_text;
            msg.images = prompt_images;
        } else {
            // 10b. Fallback: forward images from the parent session's most
            //      recent user message that contains images.  This ensures
            //      vision/painter sub-agents can actually "see" the image the
            //      user provided, instead of receiving a text-only prompt.
            if let Ok(parent_messages) = self.session_mgr.get_messages(parent_session_id) {
                for m in parent_messages.iter().rev() {
                    if !m.images.is_empty() {
                        msg.images = m.images.clone();
                        break;
                    }
                }
            }
        }

        // Create forwarding task: agent_tx → bus with session context
        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(64);
        let bus = self.bus.clone();
        let sid = sub_session_id.clone();
        let agent_name = subagent_type.to_string();
        let parent_sid = Some(parent_session_id.to_string());
        let parent_name = self
            .active_agents
            .get(parent_session_id)
            .map(|a| a.agent_name.clone());
        tokio::spawn(async move {
            while let Some(event) = agent_rx.recv().await {
                let frame = AgentEventFrame {
                    event,
                    session_id: sid.clone(),
                    agent_name: agent_name.clone(),
                    parent_session_id: parent_sid.clone(),
                    parent_session_name: parent_name.clone(),
                };
                bus.publish(BusEvent::Frame(frame));
            }
        });

        let provider = provider.clone();
        let tool_registry =
            filter_tools_for_sub_agent(&self.tool_registry, &agent_def.allowed_sub_agents);
        let rule_engine = self.rule_engine.clone();
        let session_mgr = self.session_mgr.clone();
        let mut config = self.agent_config.clone();

        // Apply agent-level steps override as hard_limit for this sub-agent
        if let Some(steps) = agent_def.steps {
            config.hard_limit = steps;
            // Also set soft_limit to 80% of hard_limit if it would exceed hard_limit
            if config.soft_limit == 0 || config.soft_limit >= steps {
                config.soft_limit = steps.saturating_mul(4) / 5;
            }
        }

        // ── W1-S3c: 创建 visp.subagent.spawn span ─────────────────────────
        let spawn_span = tracing::info_span!(
            "visp.subagent.spawn",
            visp.subagent.name = %subagent_type,
            visp.subagent.session_id = %sub_session_id,
            visp.subagent.call_id = %call_id,
            visp.subagent.task_id = tracing::field::Empty,
            visp.subagent.depth = depth,
            visp.agent.parent_session_id = %ctx.parent_session_id,
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
            trace_state = tracing::field::Empty,
            // Langfuse trace-level fields
            langfuse.session.id = tracing::field::Empty,
            langfuse.user.id = tracing::field::Empty,
            langfuse.trace.tags = tracing::field::Empty,
            langfuse.trace.name = tracing::field::Empty,
            langfuse.environment = tracing::field::Empty,
            langfuse.release = tracing::field::Empty,
            langfuse.version = tracing::field::Empty,
            langfuse.trace.public = tracing::field::Empty,
            langfuse.trace.metadata = tracing::field::Empty,
        );

        // W2-S5: Rebuild OTel parent Context from TraceContext and set it
        // on spawn_span. This must happen before the first .enter()/.in_scope(),
        // which is before tokio::spawn + .instrument().
        if let Some(tc) = trace_context.as_ref()
            && let Some(parent_ctx) = crate::observability::rebuild_parent_context(tc)
        {
            use tracing_opentelemetry::OpenTelemetrySpanExt;
            let _ = spawn_span.set_parent(parent_ctx);
        }

        // 通过 span field 传递 TraceContext（ParentLinkLayer 在 on_record 中读取）
        if let Some(tc) = trace_context.as_ref() {
            spawn_span.record("trace_id", tc.trace_id.as_str());
            if let Some(ref psid) = tc.parent_span_id {
                spawn_span.record("parent_span_id", psid.as_str());
            }
            if let Some(ref ts) = tc.trace_state {
                spawn_span.record("trace_state", ts.as_str());
            }
        }

        // 记录 Langfuse trace 级字段（仅 enabled 时写入）
        // 使用 parent_session_id 确保同 trace 内的 session/name 一致
        visp_observability::record_langfuse_trace_fields(
            &spawn_span,
            &LangfuseSpanConfig::from(&self.agent_config),
            parent_session_id,
        );

        // 记录 task_id（Risk R1：前移到 tokio::spawn 之前）
        if let Some(task_id) = _task_id {
            spawn_span.record("visp.subagent.task_id", task_id);
        }

        let loop_handle = tokio::spawn(
            async move {
                run_agent_loop(
                    provider,
                    tool_registry,
                    rule_engine,
                    session_mgr,
                    ctx,
                    &config,
                    msg,
                    agent_tx,
                )
                .await;
            }
            .instrument(spawn_span.clone()),
        );
        self.sub_agent_handles
            .insert(sub_session_id.clone(), loop_handle);

        if let Some(tx) = response_tx {
            self.pending_responses.insert(sub_session_id.clone(), tx);
        }

        tracing::info!(
            parent = parent_session_id,
            child = sub_session_id,
            agent = subagent_type,
            "sub agent spawned"
        );
    }

    /// 处理 Agent 完成
    async fn handle_done(&mut self, session_id: &str) {
        let agent_info = match self.active_agents.get(session_id) {
            Some(a) => {
                let pending_call_id = a.pending_call_id.clone();
                let parent_id = a.parent_session_id.clone();
                let agent_name = a.agent_name.clone();
                tracing::info!(
                    session_id,
                    agent_name = %a.agent_name,
                    parent_id = ?a.parent_session_id,
                    "sub-agent done received"
                );
                (pending_call_id, parent_id, agent_name)
            }
            None => {
                tracing::warn!(session_id, "handle_done: agent not in registry, ignoring");
                return;
            }
        };

        let (pending_call_id, parent_id, agent_name) = agent_info;

        // G5 收尾归属判据：主会话存在「新回合覆盖旧回合」的竞争——旧回合 `Done`
        // 迟到时若会话已被新回合改写为 `Running`，本次收尾为幂等空操作，避免移除
        // 新循环令牌 / 把状态改回 `Idle`。（子会话 id 每次 spawn 唯一，不适用。）
        if parent_id.is_none() && self.session_is_running(session_id) {
            tracing::warn!(
                session_id,
                "handle_done: stale Done for a newer running turn, ignoring"
            );
            return;
        }

        // Remove from registry
        self.active_agents.remove(session_id);
        self.sub_agent_handles.remove(session_id);

        // Finish the session
        let _ = self
            .session_mgr
            .finish_loop(session_id, SessionStatus::Idle);

        // Check pending_responses first (oneshot path)
        if let Some(tx) = self.pending_responses.remove(session_id) {
            let content = self.extract_result(session_id);
            let _ = tx.send(content);
            return;
        }

        // Send result to parent if this is a sub-agent
        if let Some(ref parent_id) = parent_id {
            let content = self.extract_result(session_id);
            let call_id = pending_call_id.unwrap_or_default();

            if let Some(parent) = self.active_agents.get(parent_id) {
                match parent
                    .inbox
                    .try_send(OrchestratorMessage::SubAgentComplete {
                        call_id,
                        content,
                        task_id: String::new(),
                    }) {
                    Ok(()) => {
                        tracing::info!(
                            session_id,
                            parent_id,
                            agent_name,
                            "sub-agent completion forwarded to parent"
                        );
                    }
                    Err(mpsc::error::TrySendError::Full(msg)) => {
                        let inbox = parent.inbox.clone();
                        let log_session_id = session_id.to_string();
                        let log_parent_id = parent_id.clone();
                        let log_agent_name = agent_name.clone();
                        tokio::spawn(async move {
                            let _ = inbox.send(msg).await;
                            tracing::info!(
                                session_id = %log_session_id,
                                parent_id = %log_parent_id,
                                agent_name = %log_agent_name,
                                "sub-agent completion forwarded to parent (after backpressure)"
                            );
                        });
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::warn!(parent_id, "parent agent inbox closed, sub result dropped");
                    }
                }
            } else {
                tracing::warn!(
                    parent_id,
                    "parent agent no longer active, sub result dropped"
                );
            }
        } else {
            // Root agent done — 会话级 `Stop`（设计 §6.2：清理置 Idle 之后）。
            //
            // 不再补发 root `Done` 帧：主 agent 的 `Done` 帧由 `agent_loop` →
            // 主转发任务 → 总线独立送达 TUI（并派生执行级 `AgentRunEnd`），
            // 此处再发会造成根回合重复的 `Done` 帧。
            let (input_tokens, output_tokens, tool_calls) =
                self.latest_usage.remove(session_id).unwrap_or((0, 0, 0));
            self.publish_stop(
                session_id,
                StopStatus::Completed,
                input_tokens,
                output_tokens,
                tool_calls,
            );
        }
    }

    /// W2: 处理 Agent 错误退出（含 panic 转发）
    ///
    /// 与 handle_done 的关键区别：
    /// - handle_done 假设 sub-agent 正常完成，向父发送 SubAgentComplete + 提取最终结果
    /// - handle_agent_error 知道 sub-agent 异常退出，向父发送 SubAgentError 携带错误消息
    ///
    /// 仍执行公共清理：从 active_agents / sub_agent_handles 移除、finish_loop。
    ///
    /// 注：当 code == AgentErrorCode::Cancelled（用户主动取消）时：
    /// - 错误消息统一为 "agent cancelled"（去除 "Agent loop cancelled" 等异种文本）
    /// - 日志降级为 info!（用户取消是预期行为，不该污染 ERROR 日志）
    async fn handle_agent_error(
        &mut self,
        session_id: &str,
        code: AgentErrorCode,
        error_message: String,
    ) {
        let is_cancel = matches!(code, AgentErrorCode::Cancelled);
        // 取消路径下统一错误文本，便于父 agent / 日志侧识别
        let normalized_message = if is_cancel {
            "agent cancelled".to_string()
        } else {
            error_message
        };

        let agent_info = match self.active_agents.get(session_id) {
            Some(a) => {
                let pending_call_id = a.pending_call_id.clone();
                let parent_id = a.parent_session_id.clone();
                let agent_name = a.agent_name.clone();
                if is_cancel {
                    tracing::info!(
                        session_id,
                        agent_name = %a.agent_name,
                        parent_id = ?a.parent_session_id,
                        "agent cancelled by user"
                    );
                } else {
                    tracing::error!(
                        session_id,
                        agent_name = %a.agent_name,
                        parent_id = ?a.parent_session_id,
                        error = %normalized_message,
                        "sub-agent error received"
                    );
                }
                (pending_call_id, parent_id, agent_name)
            }
            None => {
                tracing::warn!(
                    session_id,
                    "handle_agent_error: agent not in registry, ignoring"
                );
                return;
            }
        };

        let (pending_call_id, parent_id, agent_name) = agent_info;

        // G5 收尾归属判据：同 `handle_done`，旧回合错误迟到且会话已被新回合改写为
        // `Running` 时，本次收尾为幂等空操作（仅主会话适用）。
        if parent_id.is_none() && self.session_is_running(session_id) {
            tracing::warn!(
                session_id,
                "handle_agent_error: stale error for a newer running turn, ignoring"
            );
            return;
        }

        self.active_agents.remove(session_id);
        self.sub_agent_handles.remove(session_id);
        let _ = self
            .session_mgr
            .finish_loop(session_id, SessionStatus::Error);

        // Check pending_responses first (oneshot path)
        if let Some(tx) = self.pending_responses.remove(session_id) {
            let _ = tx.send(format!("[SubAgent Error] {}", normalized_message));
            return;
        }

        if let Some(ref parent_id) = parent_id {
            let call_id = pending_call_id.unwrap_or_default();
            self.send_sub_agent_error(parent_id, &call_id, &normalized_message)
                .await;
            tracing::info!(
                session_id,
                parent_id,
                agent_name,
                "sub-agent error forwarded to parent as SubAgentError"
            );
        } else if is_cancel {
            tracing::info!(session_id, agent_name, "root agent cancelled by user");
            // 用户取消：会话级 `Stop(status=cancelled)`，**不**误入 `StopFailure`
            // （设计 §6.2：#18）。
            let (input_tokens, output_tokens, tool_calls) =
                self.latest_usage.remove(session_id).unwrap_or((0, 0, 0));
            self.publish_stop(
                session_id,
                StopStatus::Cancelled,
                input_tokens,
                output_tokens,
                tool_calls,
            );
        } else {
            // Root agent error — Error 帧仍由 run_agent_loop 直送 CLI；
            // 会话级 `StopFailure`（设计 §6.2：语义收敛后，含 code/message）。
            tracing::error!(
                session_id,
                agent_name,
                "root agent errored: {normalized_message}"
            );
            self.publish_stop_failure(session_id, &format!("{code:?}"), &normalized_message);
        }
    }

    /// 提取 Agent 的最终结果（最后一条 assistant 消息内容）
    fn extract_result(&self, session_id: &str) -> String {
        if let Ok(session) = self.session_mgr.get(session_id) {
            for msg in session.history.iter().rev() {
                if msg.role == visp_core::message::Role::Assistant && !msg.content.is_empty() {
                    return msg.content.clone();
                }
            }
        }
        String::new()
    }

    /// 取消 Agent（递归取消所有子孙）
    fn cancel_agent(&self, session_id: &str) {
        if let Some(agent) = self.active_agents.get(session_id) {
            agent.cancel_token.cancel();
        }
        // 同时通过 session_mgr 取消 AgentLoopContext 使用的 token
        // （ActiveAgent.cancel_token 与 AgentLoopContext.cancel_token 是两个不同的实例，
        //  只取消前者对 run_agent_loop 无效）
        self.session_mgr.cancel_agent(session_id);
        for child in self.active_agents.descendants_of(session_id) {
            child.cancel_token.cancel();
            self.session_mgr.cancel_agent(&child.session_id);
        }
    }

    // ── 辅助方法 ─────────────────────────────────────────────

    /// 并发上限。0 表示无限制（所有 subagent 立即并行执行，不排队）。
    /// 非 0 时返回配置值（至少 1，避免配置 0 时所有请求永远排队造成死锁）。
    fn concurrency_limit(&self) -> usize {
        let limit = self.agent_config.max_concurrent_subagents;
        if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        }
    }

    /// 当前运行的 subagent 数（parent_session_id 非 None 的 active agent，主 agent 不计入）
    fn active_subagent_count(&self) -> usize {
        self.active_agents
            .agents_cloned()
            .iter()
            .filter(|a| a.parent_session_id.is_some())
            .count()
    }

    /// 有空位时从队列取出下一个等待的 spawn 请求执行（FIFO）
    async fn drain_queued_spawns(&mut self) {
        if self.active_subagent_count() < self.concurrency_limit()
            && let Some(next) = self.queued_spawns.pop_front()
        {
            let QueuedSpawn {
                parent_session_id,
                call_id,
                subagent_type,
                description,
                prompt,
                task_id,
                trace_context,
                response_tx,
            } = next;
            tracing::info!(call_id = %call_id, "executing queued subagent spawn");
            self.spawn_sub_agent(
                &parent_session_id,
                &call_id,
                &subagent_type,
                &description,
                &prompt,
                task_id.as_deref(),
                trace_context,
                response_tx,
            )
            .await;
        }
    }

    /// 向父 agent 发送子 agent 错误
    async fn send_sub_agent_error(&mut self, parent_session_id: &str, call_id: &str, error: &str) {
        if let Some(parent) = self.active_agents.get(parent_session_id) {
            let _ = parent.inbox.try_send(OrchestratorMessage::SubAgentError {
                call_id: call_id.to_string(),
                error: error.to_string(),
            });
        }
    }

    /// 发布「会话忙」错误帧（显示面通路；code 语义 = `SessionBusy`）。
    ///
    /// 用于 `start_loop` 因忙（会话非 `Idle`）被拒绝时向客户端给出反馈，
    /// 不再静默丢弃；文案明确不含 `cancelled` 语义。
    fn publish_session_busy(&self, session_id: &str, agent_name: &str) {
        let frame = AgentEventFrame {
            event: AgentEvent::Error {
                code: AgentErrorCode::SessionBusy,
                message: "正在生成，请稍候".to_string(),
            },
            session_id: session_id.to_string(),
            agent_name: agent_name.to_string(),
            parent_session_id: None,
            parent_session_name: None,
        };
        self.bus.publish(BusEvent::Frame(frame));
    }

    /// G5：读取会话当前状态，判断是否处于 `Running`（收尾归属判据用）。
    fn session_is_running(&self, session_id: &str) -> bool {
        self.session_mgr
            .get(session_id)
            .map(|s| s.status == SessionStatus::Running)
            .unwrap_or(false)
    }

    /// 发布 `SessionStart`（设计 §6.2：该 session 本次 daemon 内首次 `start_loop` 成功）。
    ///
    /// `origin` 本步固定为 [`Origin::Tui`]：当前唯一客户端是 TUI-over-gRPC。
    /// `headless`/`other` 预留给未来的非 TUI 客户端，届时由 daemon 在会话被对应
    /// 客户端驱动时写入 session 并透传（设计 §6.2），此处再改为读取透传值。
    fn publish_session_start(&self, session_id: &str, ctx: &AgentLoopContext, agent_name: &str) {
        let cwd = ctx.working_dir.display().to_string();
        let event = HookEvent {
            context: HookContext {
                schema: VISP_HOOK_SCHEMA,
                hook_event_name: HookEventName::SessionStart,
                session_id: session_id.to_string(),
                cwd: cwd.clone(),
                source: session_source(ctx),
                origin: Origin::Tui,
                seq: None,
            },
            payload: HookPayload::SessionStart(SessionStartPayload {
                short_id: short_session_id(session_id),
                project_path: cwd,
                model: ctx.config.model.clone(),
                model_key: ctx.config.model_key.clone().unwrap_or_default(),
                agent_name: agent_name.to_string(),
                parent_session_id: None,
            }),
        };
        self.bus.publish(BusEvent::Hook(event));
    }

    /// 发布 `UserPromptSubmit`（主 agent 受理 `UserInput` 且 `start_loop` 成功）。
    ///
    /// 载荷默认脱敏：仅给 `prompt_chars`，不含原文；显式 `include` 由后续执行器阶段补。
    fn publish_user_prompt_submit(&self, session_id: &str, ctx: &AgentLoopContext, prompt: &str) {
        let event = HookEvent {
            context: HookContext {
                schema: VISP_HOOK_SCHEMA,
                hook_event_name: HookEventName::UserPromptSubmit,
                session_id: session_id.to_string(),
                cwd: ctx.working_dir.display().to_string(),
                source: session_source(ctx),
                origin: Origin::Tui,
                seq: None,
            },
            payload: HookPayload::UserPromptSubmit(UserPromptSubmitPayload {
                prompt_chars: prompt.chars().count() as u64,
                prompt: None,
            }),
        };
        self.bus.publish(BusEvent::Hook(event));
    }

    /// 发布会话级 `Stop`（root agent 正常完成 / 用户取消；设计 §6.2）。
    fn publish_stop(
        &self,
        session_id: &str,
        status: StopStatus,
        input_tokens: u64,
        output_tokens: u64,
        tool_calls: u64,
    ) {
        let event = HookEvent {
            context: self.hook_context(session_id, HookEventName::Stop),
            payload: HookPayload::Stop(StopPayload {
                status,
                input_tokens,
                output_tokens,
                tool_calls,
            }),
        };
        self.bus.publish(BusEvent::Hook(event));
    }

    /// 发布会话级 `StopFailure`（root agent 真错误；设计 §6.2）。
    fn publish_stop_failure(&self, session_id: &str, code: &str, message: &str) {
        let event = HookEvent {
            context: self.hook_context(session_id, HookEventName::StopFailure),
            payload: HookPayload::StopFailure(StopFailurePayload {
                code: code.to_string(),
                message: message.to_string(),
            }),
        };
        self.bus.publish(BusEvent::Hook(event));
    }

    /// 构造 hook 公共信封：`cwd` 取会话项目路径，`source` 取绑定时刻记录，
    /// `origin` 本步固定 `tui`（与 `SessionStart`/`UserPromptSubmit` 一致）。
    fn hook_context(&self, session_id: &str, hook_event_name: HookEventName) -> HookContext {
        let cwd = self
            .session_mgr
            .get(session_id)
            .map(|s| s.project_path.display().to_string())
            .unwrap_or_default();
        HookContext {
            schema: VISP_HOOK_SCHEMA,
            hook_event_name,
            session_id: session_id.to_string(),
            cwd,
            source: self
                .session_sources
                .get(session_id)
                .copied()
                .unwrap_or(SessionSource::Startup),
            origin: Origin::Tui,
            seq: None,
        }
    }

    pub fn resolve_provider(
        &self,
        agent: Option<&AgentDefinition>,
        session_id: &str,
    ) -> Option<Arc<dyn LlmProvider>> {
        // 4-tier cascade (agent.model → session.model_key → session.model → default)
        // is handled by visp_config::resolve_model_key against daemon_config.llm.models.
        let agent_model = agent.and_then(|a| a.model.as_deref());
        let session_config = self
            .session_mgr
            .get(session_id)
            .map(|s| s.config)
            .unwrap_or_default();
        let key = visp_config::resolve_model_key(agent_model, &session_config, &self.daemon_config);
        self.providers.get(&key).cloned()
    }
}

#[cfg(test)]
#[path = "orchestrator_tests.rs"]
mod tests;
