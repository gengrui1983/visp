//! ACP agent 层组装(Step 8):`initialize` / `session/new` / `session/prompt` /
//! `session/cancel` 四个入口,以及 `session/prompt` 的事件泵(长任务,经
//! `cx.spawn` 卸载——SDK 事件循环为单任务,handler 内阻塞会吞掉 cancel 通知)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, ContentBlock, ContentChunk,
    CreateElicitationRequest, ElicitationAction, ElicitationFormMode, ElicitationMode,
    ElicitationScope, ElicitationSessionScope, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::util::internal_error;
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, Error as AcpError, Responder, on_receive_notification,
    on_receive_request,
};
use anyhow::anyhow;
use tokio::sync::mpsc;
use tonic::transport::Channel;
use visp_proto::visp::LlmConfig;
use visp_proto::visp::ServerMessage;
use visp_proto::visp::coder_daemon_client::CoderDaemonClient;

use crate::approval::{self, QuestionPath};
use crate::grpc::GrpcSession;
use crate::sessions::{Route, SessionRegistry, route};
use crate::translate::{Outbound, TranslateCtx, translate};

/// cancel 请求后的兜底收尾窗口(§6.6:超窗仍无收尾信号 → 自行 `cancelled`)。
const CANCEL_FALLBACK: Duration = Duration::from_secs(10);

/// 事件泵轮询窗口(兜底检查粒度)。
const PUMP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// agent 共享状态(handler 与事件泵之间)。
pub struct AgentState {
    outbound: crate::grpc::Outbound,
    inbound: tokio::sync::Mutex<mpsc::Receiver<ServerMessage>>,
    client: tokio::sync::Mutex<CoderDaemonClient<Channel>>,
    registry: tokio::sync::Mutex<SessionRegistry>,
    /// cancel 请求时刻(兜底计时起点;None = 无待收尾取消)
    cancel_requested_at: tokio::sync::Mutex<Option<Instant>>,
    /// 当前 turn 的取消令牌(session/cancel 时触发,唤醒审批/提问等待)
    cancel_token: tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
    /// cancel 兜底收尾窗口(生产 `CANCEL_FALLBACK`;测试可缩短)
    cancel_fallback: Duration,
    /// client 是否支持 `elicitation.form`(initialize 时记录,§7.5)
    supports_elicitation_form: AtomicBool,
    request_seq: AtomicU64,
    /// 新会话的初始 LLM 配置(来自启动参数;ACP v1 无模型选择请求)。
    llm_config: Option<LlmConfig>,
}

impl AgentState {
    pub fn new(
        outbound: crate::grpc::Outbound,
        inbound: mpsc::Receiver<ServerMessage>,
        client: CoderDaemonClient<Channel>,
    ) -> Self {
        Self {
            outbound,
            inbound: tokio::sync::Mutex::new(inbound),
            client: tokio::sync::Mutex::new(client),
            registry: tokio::sync::Mutex::new(SessionRegistry::new()),
            cancel_requested_at: tokio::sync::Mutex::new(None),
            cancel_token: tokio::sync::Mutex::new(None),
            cancel_fallback: CANCEL_FALLBACK,
            supports_elicitation_form: AtomicBool::new(false),
            request_seq: AtomicU64::new(1),
            llm_config: None,
        }
    }

    /// 指定新会话的初始 LLM 配置(启动参数;非 ACP 协议消息)。
    #[must_use]
    pub fn with_llm_config(mut self, config: Option<LlmConfig>) -> Self {
        self.llm_config = config;
        self
    }

    /// 覆盖 cancel 兜底窗口(仅测试用;生产保持 `CANCEL_FALLBACK`)。
    #[doc(hidden)]
    pub fn with_cancel_fallback(mut self, fallback: Duration) -> Self {
        self.cancel_fallback = fallback;
        self
    }

    /// 供集成冒烟:懒连接的空 state(`initialize` 类用例不触 gRPC)。
    /// outbound 消息进黑洞,入站为空流。`#[doc(hidden)]`——非生产 API。
    #[doc(hidden)]
    pub fn new_unconnected() -> Arc<Self> {
        let channel =
            Channel::builder("http://127.0.0.1:1".parse().expect("valid uri")).connect_lazy();
        let client = CoderDaemonClient::new(channel);
        let (tx, _rx) = mpsc::channel(8);
        Arc::new(Self::new(
            crate::grpc::Outbound::from_tx(tx),
            mpsc::channel(1).1,
            client,
        ))
    }

    fn record_client_caps(&self, caps: &ClientCapabilities) {
        let supports = caps.elicitation.as_ref().is_some_and(|e| e.supports_form());
        self.supports_elicitation_form
            .store(supports, Ordering::SeqCst);
    }

    fn supports_form(&self) -> bool {
        self.supports_elicitation_form.load(Ordering::SeqCst)
    }

    fn next_request_id(&self) -> String {
        format!("r-{}", self.request_seq.fetch_add(1, Ordering::SeqCst))
    }

    async fn create_visp_session(&self, project_path: &str) -> anyhow::Result<String> {
        let client = self.client.lock().await;
        GrpcSession::create_session_with_config(&client, project_path, self.llm_config.clone())
            .await
    }

    async fn handle_cancel(&self, notif: &CancelNotification) {
        // 触发取消令牌(唤醒审批/提问等待,SentRequest drop 自动 $/cancel_request)+
        // 兜底计时起点;daemon 侧对未运行 session 天然 no-op
        let token = self.cancel_token.lock().await.clone();
        if let Some(t) = token {
            t.cancel();
        }
        *self.cancel_requested_at.lock().await = Some(Instant::now());
        if let Err(e) = self
            .outbound
            .send_cancel(notif.session_id.0.to_string().as_str())
            .await
        {
            tracing::warn!(error = %e, "send cancel failed");
        }
    }

    /// `session/prompt`:校验 → 发 `UserInput` → 事件泵 spawn(立即返回,
    /// 响应在 turn 收尾时经 responder 发出)。
    async fn begin_prompt(
        self: &Arc<Self>,
        req: PromptRequest,
        responder: agent_client_protocol::Responder<PromptResponse>,
        cx: ConnectionTo<Client>,
    ) -> Result<(), AcpError> {
        let acp_sid: SessionId = req.session_id.clone();
        let visp_sid = acp_sid.0.to_string();
        {
            let mut registry = self.registry.lock().await;
            registry
                .try_begin_prompt(&visp_sid)
                .map_err(|busy| internal_error(format!("prompt rejected: {busy:?}")))?;
        }
        let text = prompt_text(&req)?;
        let request_id = self.next_request_id();
        self.outbound
            .send_user_input(&visp_sid, &text, &request_id)
            .await
            .map_err(|e| internal_error(e.to_string()))?;
        // 新 turn:新的取消令牌
        let token = tokio_util::sync::CancellationToken::new();
        *self.cancel_token.lock().await = Some(token.clone());
        *self.cancel_requested_at.lock().await = None;

        let state = self.clone();
        let task_cx = cx.clone();
        cx.spawn(
            async move { prompt_task(state, acp_sid, visp_sid, responder, task_cx, token).await },
        )
        .map_err(|e| internal_error(e.to_string()))
    }
}

/// 提取 prompt 文本(M1 仅文本块,顺序拼接);含非文本块或空输入 → 明确错误。
fn prompt_text(req: &PromptRequest) -> Result<String, AcpError> {
    let mut text = String::new();
    let mut has_non_text = false;
    for block in &req.prompt {
        match block {
            ContentBlock::Text(t) => text.push_str(&t.text),
            _ => has_non_text = true,
        }
    }
    if has_non_text {
        return Err(internal_error(
            "M1 仅支持文本 prompt(非文本块未在 capabilities 中声明)",
        ));
    }
    if text.is_empty() {
        return Err(internal_error("empty prompt"));
    }
    Ok(text)
}

/// `initialize` 应答:协议版本回显 + M1 capabilities(§7.5)+ 空 authMethods + agentInfo。
fn initialize_response(req: InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(req.protocol_version)
        .agent_capabilities(AgentCapabilities::new())
        .agent_info(Implementation::new("visp", env!("CARGO_PKG_VERSION")))
}

/// `session/prompt` 事件泵:消费 daemon 入站 → 归属路由 → 翻译 → ACP 下发,
/// 直至 turn 收尾(Done / Error{Cancelled} / 兜底超时)。
async fn prompt_task(
    state: Arc<AgentState>,
    acp_sid: SessionId,
    visp_sid: String,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    token: tokio_util::sync::CancellationToken,
) -> Result<(), AcpError> {
    let mut ctx = TranslateCtx::new(&visp_sid);
    loop {
        // 收事件(轮询窗口内等一条;窗口到期继续兜底检查)
        let inbound_msg = {
            let mut inbound = state.inbound.lock().await;
            tokio::time::timeout(PUMP_POLL_INTERVAL, inbound.recv()).await
        };
        match inbound_msg {
            Ok(Some(msg)) => {
                if let Some(reason) = dispatch(msg, &state, &mut ctx, &acp_sid, &cx, &token).await?
                {
                    state.registry.lock().await.end_prompt(&visp_sid);
                    return responder.respond(PromptResponse::new(reason));
                }
            }
            Ok(None) => {
                // daemon 流断开 → 在途 prompt 以错误结束(§8 表),不静默悬挂
                state.registry.lock().await.end_prompt(&visp_sid);
                return responder.respond(PromptResponse::new(StopReason::Refusal));
            }
            Err(_elapsed) => {
                // 轮询窗口到期:兜底检查(cancel 请求超窗仍无收尾信号 → 自行
                // cancelled;core 修复落地后此路径仅作防御层,§6.6)
                let cancel_elapsed = {
                    let cancel = state.cancel_requested_at.lock().await;
                    cancel.is_some_and(|t| t.elapsed() >= state.cancel_fallback)
                };
                if cancel_elapsed {
                    // 兜底窗口到期仍无终态信号:补发一次 cancel(让 daemon 侧尽快
                    // 收敛,而非适配器单方面结束),再自行以 cancelled 收尾
                    // (§6.6 防御层,core 修复落地后此路径仅作纵深)。
                    if let Err(e) = state.outbound.send_cancel(&visp_sid).await {
                        tracing::warn!(error = %e, "fallback send cancel failed");
                    }
                    state.registry.lock().await.end_prompt(&visp_sid);
                    return responder.respond(PromptResponse::new(StopReason::Cancelled));
                }
            }
        }
    }
}

/// 处理一条入站事件;返回 `Some(stop_reason)` 表示 turn 应收尾。
async fn dispatch(
    msg: ServerMessage,
    state: &Arc<AgentState>,
    ctx: &mut TranslateCtx,
    acp_sid: &SessionId,
    cx: &ConnectionTo<Client>,
    token: &tokio_util::sync::CancellationToken,
) -> Result<Option<StopReason>, AcpError> {
    // 归属路由:UserQuery 旁路进桥接(§8.4 特例);无在途的迟到事件丢弃
    let msg = match route(msg, &*state.registry.lock().await) {
        Route::Drop => return Ok(None),
        Route::Bridge(m) | Route::Pump(m) => m,
    };
    for out in translate(&msg, ctx) {
        match out {
            Outbound::Update(update) => {
                cx.send_notification(SessionNotification::new(acp_sid.clone(), *update))
                    .map_err(|e| internal_error(e.to_string()))?;
            }
            Outbound::ErrorText(text) => {
                // 错误信息作为文本 chunk 下发,不静默丢弃(§6.4 步骤 6)
                cx.send_notification(SessionNotification::new(
                    acp_sid.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(text),
                    ))),
                ))
                .map_err(|e| internal_error(e.to_string()))?;
            }
            Outbound::ApprovalNeeded(q) => {
                // 7a:session/request_permission → V4 索引回填(§6.5)。
                // turn 收尾由后续 Done/Error 信号驱动:父审批 cancelled ≈ 取消中,
                // 子审批 cancelled 仅等价拒绝(三轮修正)。
                let req = RequestPermissionRequest::new(
                    acp_sid.clone(),
                    approval::synthetic_tool_call(&q),
                    approval::permission_options(),
                );
                let sent = cx.send_request(req);
                let idx = tokio::select! {
                    biased;
                    // 取消:SentRequest drop 自动发 $/cancel_request;daemon 侧将
                    // pending UserQuery 置 -1(V5),无需回填,继续等 Error{Cancelled}
                    _ = token.cancelled() => continue,
                    resp = sent.block_task() => match resp {
                        Ok(r) => match r.outcome {
                            RequestPermissionOutcome::Selected(sel) => {
                                approval::map_selected_option(sel.option_id.0.to_string().as_str())
                            }
                            RequestPermissionOutcome::Cancelled => -1,
                            // non_exhaustive:未知结果一律 deny(§6.5「其余任何值一律 deny」)
                            _ => -1,
                        },
                        Err(_) => -1,
                    },
                };
                state
                    .outbound
                    .send_user_response(&q.query_id, idx, "")
                    .await
                    .map_err(|e| internal_error(e.to_string()))?;
            }
            Outbound::QuestionNeeded(q) => {
                // 7b:elicitation/create(支持 form)或降级;任何路径必须回填(V12)
                match approval::question_path(state.supports_form()) {
                    QuestionPath::Elicitation => {
                        let mode = ElicitationMode::Form(ElicitationFormMode::new(
                            ElicitationScope::Session(ElicitationSessionScope::new(
                                acp_sid.clone(),
                            )),
                            approval::question_schema(&q),
                        ));
                        let req = CreateElicitationRequest::new(mode, q.message.clone());
                        let sent = cx.send_request(req);
                        let (idx, text) = tokio::select! {
                            biased;
                            // 取消:SentRequest drop 自动 $/cancel_request;不回填,
                            // 继续 等 Error{Cancelled}(阻断 1 场景的正确收尾)
                            _ = token.cancelled() => continue,
                            resp = sent.block_task() => match resp {
                                Ok(r) => match r.action {
                                    ElicitationAction::Accept(a) => match a.content {
                                        Some(content) => {
                                            approval::map_elicitation_accept(&content, &q.options)
                                        }
                                        None => approval::map_elicitation_reject(),
                                    },
                                    _ => approval::map_elicitation_reject(),
                                },
                                Err(_) => approval::map_elicitation_reject(),
                            },
                        };
                        state
                            .outbound
                            .send_user_response(&q.query_id, idx, &text)
                            .await
                            .map_err(|e| internal_error(e.to_string()))?;
                    }
                    QuestionPath::Downgrade => {
                        let (idx, text) = approval::map_downgrade(&q);
                        // 问题原文作为文本 chunk 下发(降级可见,§6.4 步骤 7)
                        cx.send_notification(SessionNotification::new(
                            acp_sid.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(q.message.clone())),
                            )),
                        ))
                        .map_err(|e| internal_error(e.to_string()))?;
                        state
                            .outbound
                            .send_user_response(&q.query_id, idx, &text)
                            .await
                            .map_err(|e| internal_error(e.to_string()))?;
                    }
                }
            }
            Outbound::FinishTurn(reason) => {
                return Ok(Some(reason));
            }
        }
    }
    Ok(None)
}

/// ACP agent 事件循环:注册四个入口并连接 transport(§6.1 步骤 4-5)。
pub async fn run_agent<T>(transport: T, state: Arc<AgentState>) -> anyhow::Result<()>
where
    T: ConnectTo<Agent>,
{
    let st_init = Arc::clone(&state);
    let st_new = Arc::clone(&state);
    let st_prompt = Arc::clone(&state);
    let st_cancel = state;
    Agent
        .builder()
        .name("visp-acp")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                st_init.record_client_caps(&req.client_capabilities);
                responder.respond(initialize_response(req))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                let visp_id = st_new
                    .create_visp_session(&req.cwd.to_string_lossy())
                    .await
                    .map_err(|e| internal_error(e.to_string()))?;
                {
                    let mut registry = st_new.registry.lock().await;
                    registry.register(visp_id.clone(), visp_id.clone());
                }
                responder.respond(NewSessionResponse::new(visp_id))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx| {
                st_prompt.begin_prompt(req, responder, cx.clone()).await
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: CancelNotification, _cx| {
                st_cancel.handle_cancel(&notif).await;
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| anyhow!("ACP connection error: {e}"))
}
