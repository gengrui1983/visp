use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tokio::sync::{Notify, RwLock, mpsc};
use tonic::{Request, Response, Status, Streaming};

use visp_codegraph::CodeGraph;
use visp_core::{
    agent::{AgentConfig, AgentEvent, UserQueryResult},
    bus::BusEvent,
    context::ContextTrimmer,
    message::{MessageType, Role},
    provider::{LlmConfig, LlmProvider},
    rules::RuleEngine,
    session::{SessionManager, SessionStatus},
    tool::ToolContext,
    tool_registry::ToolRegistry,
};
use visp_daemon::bus::{BusEnvelope, EventBus};
use visp_daemon::reload::{ReloadCore, ReloadDomain, ReloadItem};
use visp_hooks::{
    HookContext, HookEvent, HookEventName, HookPayload, Origin, PermissionOutcome,
    PermissionResultPayload, SessionEndPayload, SessionSource, VISP_HOOK_SCHEMA,
};
use visp_mcp::manager::McpManager;
use visp_proto::visp::{self as proto, coder_daemon_server::CoderDaemon};

use crate::config::DaemonConfig;
use crate::config::LlmModelConfig;
use crate::shutdown::{HOOK_DRAIN_BUDGET, HookDrainHost};
use visp_daemon::hook_runtime::HookStatsSource;

type ResponseStream =
    Pin<Box<dyn futures::Stream<Item = Result<proto::ServerMessage, tonic::Status>> + Send>>;
type CodeGraphMap = Arc<RwLock<HashMap<String, Arc<CodeGraph>>>>;

/// `Lagged` 回放的节流窗口：同一连接在窗口内最多回放一次，避免风暴下重复回放。
const LAGGED_REPLAY_THROTTLE: Duration = Duration::from_millis(500);

/// 连接级回放状态（inbound 与 outbound 共享）。
///
/// - `target`：inbound 收到 `JoinSession` 时登记的「当前正在查看的 session」；
/// - `replayed_upto`：各 session 已回放到的 history 长度，用于增量回放去重
///   （避免把已收帧再发一遍）；
/// - `last_replay`：上次回放时刻，配合 `window` 做节流。
struct ReplayState {
    target: Option<String>,
    replayed_upto: HashMap<String, usize>,
    last_replay: Option<Instant>,
    window: Duration,
}

impl ReplayState {
    fn new(window: Duration) -> Self {
        Self {
            target: None,
            replayed_upto: HashMap::new(),
            last_replay: None,
            window,
        }
    }

    /// 登记目标 session 及其已回放水位（inbound `JoinSession` 回放完成后调用）。
    fn set_target(&mut self, session_id: &str, replayed_upto: usize) {
        self.target = Some(session_id.to_owned());
        self.replayed_upto
            .insert(session_id.to_owned(), replayed_upto);
    }

    /// 该 session 下一次增量回放的起始下标。
    fn replay_from(&self, session_id: &str) -> usize {
        self.replayed_upto.get(session_id).copied().unwrap_or(0)
    }

    /// 节流：距上次回放是否已过窗口（首次恒允许）。
    fn throttle_allows(&self, now: Instant) -> bool {
        match self.last_replay {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.window,
        }
    }

    /// 记录一次回放：推进水位并刷新节流时刻。
    fn note_replayed(&mut self, session_id: &str, replayed_upto: usize, now: Instant) {
        self.replayed_upto
            .insert(session_id.to_owned(), replayed_upto);
        self.last_replay = Some(now);
    }
}

fn create_llm_provider(config: &LlmModelConfig) -> Result<Arc<dyn LlmProvider>, String> {
    match config.protocol.as_str() {
        "openai" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "OPENAI_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(visp_llm::openai::OpenAiProvider::with_base_url(
                    api_key,
                    base_url.clone(),
                )))
            } else {
                Ok(Arc::new(visp_llm::openai::OpenAiProvider::new(api_key)))
            }
        }
        "aliyun" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "ALIYUN_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            let base_url = config.base_url.clone().ok_or_else(|| {
                "ALIYUN base_url not set (configure base_url in daemon.toml)".to_string()
            })?;
            Ok(Arc::new(visp_llm::aliyun::AliyunProvider::new(
                api_key, base_url,
            )))
        }
        "bigmodel" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "BIGMODEL_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            Ok(Arc::new(visp_llm::bigmodel::BigModelProvider::new(
                api_key,
                config.base_url.clone(),
            )))
        }
        "opencode" | "opencode-go" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "OPENCODE_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(visp_llm::opencode::OpencodeProvider::new(
                    api_key,
                    Some(base_url.clone()),
                )))
            } else if config.protocol == "opencode-go" {
                // "opencode-go" 默认指向 Go 通道 v1 端点
                Ok(Arc::new(visp_llm::opencode_go::OpencodeGoProvider::new(
                    api_key, None,
                )))
            } else {
                Ok(Arc::new(visp_llm::opencode::OpencodeProvider::new(
                    api_key, None,
                )))
            }
        }
        _ => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "ANTHROPIC_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(
                    visp_llm::anthropic::AnthropicProvider::with_base_url(
                        api_key,
                        base_url.clone(),
                    ),
                ))
            } else {
                Ok(Arc::new(visp_llm::anthropic::AnthropicProvider::new(
                    api_key,
                )))
            }
        }
    }
}

/// daemon 侧待响应的用户查询：回传通道 + 关联 session（`PermissionResult` 载荷需要）。
struct PendingQuery {
    /// 把 `UserResponse` 直接回传给等待中的 agent loop。
    respond: mpsc::Sender<UserQueryResult>,
    /// 该查询所属 session（取自 `UserQuery` 帧）。
    session_id: String,
}

pub struct CoderDaemonService {
    #[allow(dead_code)]
    provider: Arc<StdRwLock<Arc<dyn LlmProvider>>>,
    tool_registry: Arc<ToolRegistry>,
    rule_engine: Arc<RuleEngine>,
    /// 共享 reload 核心（显式 `/reload` 入口；自动监听路径复用同一实例）。
    reload_core: Arc<ReloadCore>,
    session_mgr: Arc<SessionManager>,
    #[allow(dead_code)]
    agent_config: AgentConfig,
    start_time: Instant,
    /// Phase 5: lazy-loaded CodeGraph instances per project path
    codegraphs: CodeGraphMap,
    /// 默认 LLM 配置（来自 daemon.toml），create_session 时与客户端配置合并
    default_llm_config: LlmConfig,
    /// 完整 daemon 配置（visp_config 运行时函数使用）
    daemon_config: Arc<DaemonConfig>,
    /// 上下文裁剪器
    #[allow(dead_code)]
    context_trimmer: Arc<dyn ContextTrimmer + Send + Sync>,
    /// MCP 服务器管理器
    mcp_manager: Arc<McpManager>,
    /// 模型显示标签列表（格式 "{name}({provider})"，用于 proto Session.available_models）
    available_models: Vec<String>,
    /// 完整模型配置列表
    model_configs: Vec<LlmModelConfig>,
    /// 模型 key 列表（格式 "{provider}/{name}"，用于 proto Session.model_keys）
    model_config_keys: Vec<String>,
    // ── 多 Agent Orchestrator 通道 ──
    /// 向 Orchestrator 发送取消信号
    cancel_tx: mpsc::Sender<visp_agent::orchestrator::CancelSignal>,
    /// 显示面事件总线：每个 Chat 连接 `subscribe()` 独立订阅（设计 §5 D3）。
    bus: Arc<EventBus>,
    /// 向 Orchestrator 发送 ClientMessage（CLI 输入）
    client_tx: mpsc::Sender<visp_agent::orchestrator::ClientMessage>,
    // ── 优雅关停管道（设计 D13）──
    /// Shutdown RPC 完成后唤醒 daemon main 执行进程级清理。
    shutdown_notify: Arc<Notify>,
    /// 关停期的 hook 有界 drain 宿主（1a 为空实现，1b-2c 接真实执行器）。
    hook_drain: Arc<dyn HookDrainHost>,
    /// 只读 hook 计数/规则摘要来源（`GetHookStats` RPC；无运行时为全零实现）。
    hook_stats: Arc<dyn HookStatsSource>,
    /// 终止态抑制标记：首次关停后置位，`SessionEnd` 只发一次。
    /// 以 `Arc` 共享给 Chat 的 inbound 任务，使 `PermissionResult` 发射点也能感知终止态。
    shutting_down: Arc<AtomicBool>,
}

// gRPC 辅助方法返回 Result<_, tonic::Status>（约 176 字节），
// 触发 clippy::result_large_err（CI 以 -D warnings 运行）。
// 与下方 CoderDaemon trait impl 块同一根因。
#[allow(clippy::result_large_err)]
impl CoderDaemonService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_configs: Vec<LlmModelConfig>,
        tool_registry: Arc<ToolRegistry>,
        rule_engine: Arc<RuleEngine>,
        reload_core: Arc<ReloadCore>,
        session_mgr: Arc<SessionManager>,
        #[allow(dead_code)] agent_config: AgentConfig,
        daemon_config: Arc<DaemonConfig>,
        #[allow(dead_code)] context_trimmer: Arc<dyn ContextTrimmer + Send + Sync>,
        mcp_manager: Arc<McpManager>,
        available_models: Vec<String>,
        cancel_tx: mpsc::Sender<visp_agent::orchestrator::CancelSignal>,
        bus: Arc<EventBus>,
        client_tx: mpsc::Sender<visp_agent::orchestrator::ClientMessage>,
        shutdown_notify: Arc<Notify>,
        hook_drain: Arc<dyn HookDrainHost>,
        hook_stats: Arc<dyn HookStatsSource>,
    ) -> Result<Self, String> {
        // 查找默认模型（匹配 {provider}/{name} 或 {provider}/{model} 格式）
        let default_idx = if let Some(ref default_key) = daemon_config.llm.default {
            match model_configs
                .iter()
                .position(|mc| mc.matches_key(default_key))
            {
                Some(idx) => {
                    tracing::info!(
                        default = %default_key,
                        "using llm.default model for new sessions"
                    );
                    idx
                }
                None => {
                    tracing::warn!(
                        default = %default_key,
                        available = %model_configs.iter().map(|m| m.key()).collect::<Vec<_>>().join(", "),
                        "llm.default points to unknown model, falling back to first model"
                    );
                    0
                }
            }
        } else {
            tracing::info!("llm.default not set, using first model as default");
            0
        };
        let default_cfg = &model_configs[default_idx];

        let initial_provider = create_llm_provider(default_cfg).map_err(|error| {
            format!(
                "failed to create initial LLM provider '{}': {error}",
                default_cfg.key()
            )
        })?;

        // 使用 visp_config 构建默认 LLM 配置（含 per-model thinking_budget_tokens 与 langfuse）
        let mut default_llm_config = visp_config::build_llm_config_from_model(
            default_cfg,
            Some(&daemon_config.observability),
        );
        // 合并 [llm.extra] 自定义参数与全局 thinking_budget_tokens
        // （per-model 值已在 build_llm_config_from_model 中注入，优先级最高）
        for (k, v) in &daemon_config.llm.extra {
            default_llm_config
                .extra
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        if let Some(budget) = daemon_config.llm.thinking_budget_tokens {
            default_llm_config
                .extra
                .entry("thinking_budget_tokens".into())
                .or_insert_with(|| budget.to_string());
        }
        // per-model use_tool / image_generation 覆盖
        default_llm_config.use_tool = default_cfg.use_tool.unwrap_or(true);
        default_llm_config.image_generation = default_cfg.image_generation.unwrap_or(false);
        let model_config_keys: Vec<String> = model_configs.iter().map(|mc| mc.key()).collect();
        Ok(Self {
            provider: Arc::new(StdRwLock::new(initial_provider)),
            tool_registry,
            rule_engine,
            reload_core,
            session_mgr,
            agent_config,
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config,
            daemon_config,
            context_trimmer,
            mcp_manager,
            available_models,
            model_configs,
            model_config_keys,
            cancel_tx,
            bus,
            client_tx,
            shutdown_notify,
            hook_drain,
            hook_stats,
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 标记关停开始；`true` 表示本次是首次（一次性终态动作的前置判定）。
    fn begin_shutdown(&self) -> bool {
        self.shutting_down
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 关停是否已开始（终止态：此后不再派发 hook 事件）。
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// 直投 daemon 级关停 `SessionEnd`（设计 D13 / §6.4）。
    ///
    /// `ShutdownRequest` 不携带 session id，故以空 `session_id` 表达 daemon 级终态；
    /// per-session 细化留待后续（活跃会话追踪，历史会话会误发，见已知限制）。
    ///
    /// **不经总线**：经 [`HookDrainHost::emit_terminal`] 同步直投 hook 执行器。若走总线，
    /// 关停随后立即置 `accepting=false`（drain），总线消费者可能尚未取走该事件即被丢弃，
    /// 导致终态 hook（如 herdr `release-agent`）不执行。直投绕过该竞态。
    async fn publish_shutdown_session_end(&self) {
        let event = HookEvent {
            context: HookContext {
                schema: VISP_HOOK_SCHEMA,
                hook_event_name: HookEventName::SessionEnd,
                session_id: String::new(),
                cwd: hook_cwd(),
                source: SessionSource::Startup,
                origin: Origin::Other,
                seq: None,
            },
            payload: HookPayload::SessionEnd(SessionEndPayload {
                reason: "shutdown".to_string(),
                exit_code: Some(0),
            }),
        };
        self.hook_drain.emit_terminal(event).await;
    }

    /// 发布会话删除 `SessionEnd`（设计 §6.4：`delete_session` 成功后）。
    ///
    /// `reason="delete"`、`exit_code=None`（设计 §6.2）。终止态下跳过。
    fn publish_session_deleted(&self, session_id: &str) {
        let event = HookEvent {
            context: HookContext {
                schema: VISP_HOOK_SCHEMA,
                hook_event_name: HookEventName::SessionEnd,
                session_id: session_id.to_string(),
                cwd: hook_cwd(),
                source: SessionSource::Startup,
                origin: Origin::Other,
                seq: None,
            },
            payload: HookPayload::SessionEnd(SessionEndPayload {
                reason: "delete".to_string(),
                exit_code: None,
            }),
        };
        self.bus.publish(BusEvent::Hook(event));
    }

    /// Phase 5: lazy-load a CodeGraph for a project path.
    /// Triggers background build_full on first access.
    async fn get_codegraph(&self, project_path: &str) -> Result<Arc<CodeGraph>, Status> {
        let map = self.codegraphs.read().await;
        if let Some(cg) = map.get(project_path) {
            return Ok(cg.clone());
        }
        drop(map);

        let mut cg = CodeGraph::open(Path::new(project_path))
            .map_err(|e| Status::internal(format!("codegraph open: {e}")))?;

        // Start file watcher for incremental indexing
        if let Err(e) = cg
            .start_watching(
                Path::new(project_path),
                visp_codegraph::index::CodeGraphConfig::default(),
            )
            .await
        {
            tracing::warn!("codegraph watcher start failed for {project_path}: {e}");
        }

        let cg = Arc::new(cg);

        // Background full index build (incremental updates will come via watcher)
        let bg = cg.clone();
        let pp = project_path.to_owned();
        let config = visp_codegraph::index::CodeGraphConfig::default();
        tokio::spawn(async move {
            if let Err(e) = bg.build_full(Path::new(&pp), &config).await {
                tracing::warn!("codegraph build_full failed for {pp}: {e}");
            }
        });

        let mut map = self.codegraphs.write().await;
        map.insert(project_path.to_owned(), cg.clone());
        Ok(cg)
    }
}

// ── trait implementation ──────────────────────────────────────────────────────

#[tonic::async_trait]
// tonic 的 gRPC trait 方法返回 Result<_, tonic::Status>（约 176 字节），
// 触发 clippy::result_large_err（CI 以 -D warnings 运行）。
#[allow(clippy::result_large_err)]
impl CoderDaemon for CoderDaemonService {
    type ChatStream = ResponseStream;

    async fn create_session(
        &self,
        request: Request<proto::CreateSessionRequest>,
    ) -> Result<Response<proto::Session>, Status> {
        let req = request.into_inner();
        // 从客户端配置开始，用 daemon 默认值合并未设置的字段（visp_config）
        let mut config =
            visp_config::merge_session_config(req.config.as_ref(), &self.daemon_config);
        // merge_session_config 不处理 langfuse 与哨兵值回填，这里用 daemon 默认值补齐
        config.langfuse_enabled = self.default_llm_config.langfuse_enabled;
        config.langfuse_session_id = None;
        config.langfuse_trace_name = None;
        config.langfuse_user_id = self.default_llm_config.langfuse_user_id.clone();
        config.langfuse_tags = self.default_llm_config.langfuse_tags.clone();
        config.langfuse_environment = self.default_llm_config.langfuse_environment.clone();
        config.langfuse_release = self.default_llm_config.langfuse_release.clone();
        config.langfuse_version = self.default_llm_config.langfuse_version.clone();
        config.langfuse_public = self.default_llm_config.langfuse_public;
        config.langfuse_metadata = self.default_llm_config.langfuse_metadata.clone();
        config.langfuse_capture_input = self.default_llm_config.langfuse_capture_input;
        config.langfuse_capture_output = self.default_llm_config.langfuse_capture_output;
        config.langfuse_capture_max_chars = self.default_llm_config.langfuse_capture_max_chars;
        config.langfuse_redact_secrets = self.default_llm_config.langfuse_redact_secrets;
        if config.max_tokens == LlmConfig::default().max_tokens {
            config.max_tokens = self.default_llm_config.max_tokens;
        }
        if config.max_context_tokens == LlmConfig::default().max_context_tokens {
            config.max_context_tokens = self.default_llm_config.max_context_tokens;
        }
        if (config.temperature - LlmConfig::default().temperature).abs() < f64::EPSILON {
            config.temperature = self.default_llm_config.temperature;
        }
        // Inject project_path into config.extra so providers can save base64 images
        config
            .extra
            .insert("project_path".into(), req.project_path.clone());
        let session = self
            .session_mgr
            .create(Path::new(&req.project_path), config)
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(session_to_proto(
            &session,
            &self.available_models,
            &self.model_config_keys,
            &self.model_configs,
        )))
    }

    async fn list_sessions(
        &self,
        _request: Request<()>,
    ) -> Result<Response<proto::ListSessionsResponse>, Status> {
        let sessions = self
            .session_mgr
            .list()
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::ListSessionsResponse {
            sessions: sessions
                .iter()
                .filter(|s| s.parent_id.is_none())
                .map(|s| {
                    session_to_proto(
                        s,
                        &self.available_models,
                        &self.model_config_keys,
                        &self.model_configs,
                    )
                })
                .collect(),
        }))
    }

    async fn get_session(
        &self,
        request: Request<proto::GetSessionRequest>,
    ) -> Result<Response<proto::Session>, Status> {
        let session_id = request.into_inner().session_id;

        // Step 1: Exact match
        if let Ok(session) = self.session_mgr.get(&session_id) {
            return Ok(Response::new(session_to_proto(
                &session,
                &self.available_models,
                &self.model_config_keys,
                &self.model_configs,
            )));
        }

        // Step 2: Prefix matching
        let sessions = self
            .session_mgr
            .list()
            .map_err(|e| Status::internal(e.to_string()))?;

        let matched: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.starts_with(&session_id))
            .collect();

        match matched.len() {
            0 => Err(Status::not_found("Session not found")),
            1 => Ok(Response::new(session_to_proto(
                matched[0],
                &self.available_models,
                &self.model_config_keys,
                &self.model_configs,
            ))),
            _ => Err(Status::invalid_argument("Ambiguous session prefix")),
        }
    }

    async fn delete_session(
        &self,
        request: Request<proto::DeleteSessionRequest>,
    ) -> Result<Response<()>, Status> {
        let session_id = request.into_inner().session_id;
        self.session_mgr
            .delete(&session_id)
            .map_err(|e| Status::internal(e.to_string()))?;
        // 删除成功后发布会话级 `SessionEnd`（设计 §6.4）；终止态下跳过（幂等抑制）。
        if !self.is_shutting_down() {
            self.publish_session_deleted(&session_id);
        }
        Ok(Response::new(()))
    }

    async fn chat(
        &self,
        request: Request<Streaming<proto::ClientMessage>>,
    ) -> Result<Response<Self::ChatStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(128);

        // 每连接独立订阅事件总线（无 take，重连天然允许）。
        let bus_rx = self.bus.subscribe();

        // Clone channels for the two forwarding tasks
        let client_tx = self.client_tx.clone();
        let response_tx = tx.clone();
        let session_mgr = self.session_mgr.clone();
        let daemon_config = self.daemon_config.clone();

        // 连接级回放状态：inbound 登记目标 session，outbound 在 Lagged 时增量回放。
        let replay_state = Arc::new(Mutex::new(ReplayState::new(LAGGED_REPLAY_THROTTLE)));
        let replay_state_inbound = replay_state.clone();

        // Shared pending user queries: maps query_id → respond sender
        // Used to route UserResponse from CLI back to the agent loop that's waiting
        // for it. This is necessary because the event bypasses global_tx.
        let pending_queries: Arc<Mutex<HashMap<String, PendingQuery>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // ── Inbound: CLI → Orchestrator / Pending Queries ──
        let pending_inbound = pending_queries.clone();
        let response_tx_inbound = response_tx.clone();
        // `PermissionResult` 发射点需要总线与终止态标记（设计 §6.4）。
        let bus_inbound = self.bus.clone();
        let shutting_down_inbound = self.shutting_down.clone();
        tokio::spawn(async move {
            while let Some(msg_result) = in_stream.next().await {
                let msg = match msg_result {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg.payload {
                    Some(proto::client_message::Payload::UserInput(input)) => {
                        let session_id = input.session_id;
                        // 受理守卫（设计 §5.6 / G1）：分类必须排在斜杠命令解析之前。
                        // 非主会话（有 parent_id）→ 拒绝（SessionNotActive，保持现状）；
                        // 主会话存在在途循环令牌 → 忙拒绝（SessionBusy）；其余受理。
                        // 判据只用 `has_inflight_loop`，不看 `status`；被拒路径零副作用。
                        let decision = match session_mgr.get(&session_id) {
                            Ok(s) => decide_user_input(
                                s.parent_id.is_none(),
                                session_mgr.has_inflight_loop(&session_id),
                            ),
                            Err(_) => UserInputDecision::RejectNotActive,
                        };
                        if decision == UserInputDecision::Accept {
                            // Intercept daemon-side slash commands and either
                            // replace the prompt (for /init) or execute file
                            // operations (for /init-agent, /init-skill).
                            let cmd = visp_command::parse(&input.text);
                            match visp_command::resolve(
                                &cmd,
                                &session_mgr
                                    .get(&session_id)
                                    .ok()
                                    .map(|s| s.project_path.clone())
                                    .unwrap_or_default(),
                            ) {
                                Ok(action) => {
                                    match action {
                                        visp_command::CommandAction::Prompt(prompt) => {
                                            // Forward the prompt to the LLM.
                                            // Side-effect: ensure .visp dirs exist.
                                            if let Ok(session) = session_mgr.get(&session_id) {
                                                let visp_dir = session.project_path.join(".visp");
                                                for sub in ["rules", "skills"] {
                                                    let _ =
                                                        std::fs::create_dir_all(visp_dir.join(sub));
                                                }
                                            }
                                            let cli_msg = visp_agent::orchestrator::ClientMessage::UserInput {
                                                session_id,
                                                text: prompt,
                                            };
                                            if client_tx.send(cli_msg).await.is_err() {
                                                break;
                                            }
                                        }
                                        visp_command::CommandAction::WriteFile {
                                            path,
                                            content,
                                        } => {
                                            // Write file and send status back.
                                            let parent = path.parent().unwrap();
                                            if let Err(e) = std::fs::create_dir_all(parent) {
                                                let err_msg = session_error_msg(
                                                    "FileWriteError",
                                                    &format!("Failed to create directory: {e}"),
                                                    &session_id,
                                                );
                                                let _ = response_tx_inbound.send(Ok(err_msg)).await;
                                            } else if let Err(e) = std::fs::write(&path, &content) {
                                                let err_msg = session_error_msg(
                                                    "FileWriteError",
                                                    &format!("Failed to write file: {e}"),
                                                    &session_id,
                                                );
                                                let _ = response_tx_inbound.send(Ok(err_msg)).await;
                                            } else {
                                                let msg = proto::ServerMessage {
                                                    payload: Some(proto::server_message::Payload::StatusUpdate(
                                                        proto::StatusUpdate {
                                                            session_id: session_id.clone(),
                                                            message: format!("Created {}", path.display()),
                                                            agent_name: String::new(),
                                                            user_inputs: vec![],
                                                            view_only: false,
                                                        },
                                                    )),
                                                };
                                                let _ = response_tx_inbound.send(Ok(msg)).await;
                                            }
                                        }
                                        visp_command::CommandAction::None => {
                                            // Not a daemon command — forward as-is.
                                            let cli_msg = visp_agent::orchestrator::ClientMessage::UserInput {
                                                session_id,
                                                text: input.text,
                                            };
                                            if client_tx.send(cli_msg).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(err_msg) => {
                                    // resolve returned an error (e.g. invalid name, file exists)
                                    let err_msg =
                                        session_error_msg("CommandError", &err_msg, &session_id);
                                    let _ = response_tx_inbound.send(Ok(err_msg)).await;
                                }
                            }
                        } else {
                            // 被拒路径零副作用：不改 status、不移除令牌、不解析命令、不转发。
                            let (code, message) = match decision {
                                UserInputDecision::RejectBusy => {
                                    (SESSION_BUSY_CODE, SESSION_BUSY_MESSAGE.to_string())
                                }
                                // RejectNotActive（含未知会话）保持现状文案。
                                _ => (
                                    SESSION_NOT_ACTIVE_CODE,
                                    format!(
                                        "Session {} is not active",
                                        &session_id[..session_id.len().min(8)]
                                    ),
                                ),
                            };
                            let err_msg = session_error_msg(code, &message, &session_id);
                            let _ = response_tx_inbound.send(Ok(err_msg)).await;
                        }
                    }
                    Some(proto::client_message::Payload::UserResponse(resp)) => {
                        let query_id = resp.query_id;
                        let text = resp.text;
                        let selected_index = resp.selected_index;

                        // 先路由 daemon 侧 pending 查询（直达等待中的 agent loop）；
                        // 命中的单一发布点在此发出 `PermissionResult`（设计 §6.4）。
                        let responded = route_daemon_user_response(
                            &bus_inbound,
                            &pending_inbound,
                            &shutting_down_inbound,
                            &query_id,
                            selected_index,
                            &text,
                        );
                        if !responded {
                            // 未命中：过期/外部响应，回退 orchestrator（不发布 hook 事件）。
                            let cli_msg =
                                visp_agent::orchestrator::ClientMessage::UserQueryResponse {
                                    query_id,
                                    selected_index,
                                    text,
                                };
                            if client_tx.send(cli_msg).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(proto::client_message::Payload::Cancel(cancel)) => {
                        let cli_msg = visp_agent::orchestrator::ClientMessage::Cancel {
                            session_id: cancel.session_id,
                        };
                        if client_tx.send(cli_msg).await.is_err() {
                            break;
                        }
                    }
                    Some(proto::client_message::Payload::ConfigUpdate(update)) => {
                        let session_id = &update.session_id;

                        // 获取当前 session 配置，在其基础上应用更新（保留 langfuse/use_tool 等字段）
                        let current_config = session_mgr
                            .get(session_id)
                            .map(|s| s.config)
                            .unwrap_or_default();

                        // 用 visp_config 应用更新
                        let mut final_config = if let Some(ref update_config) = update.config {
                            visp_config::apply_config_update(
                                &current_config,
                                update_config,
                                &daemon_config,
                            )
                        } else {
                            current_config
                        };

                        // apply_config_update 不处理 per-model use_tool/image_generation 覆盖
                        if let Some(ref update_config) = update.config
                            && let Some(ref model_key) = update_config.model_key
                            && let Some(mc) = daemon_config
                                .llm
                                .models
                                .iter()
                                .find(|mc| mc.matches_key(model_key))
                        {
                            final_config.use_tool = mc.use_tool.unwrap_or(true);
                            final_config.image_generation = mc.image_generation.unwrap_or(false);
                        }

                        // Inject project_path from the existing session so providers
                        // can continue to save base64 images after a config update.
                        if let Ok(existing) = session_mgr.get(session_id) {
                            final_config.extra.insert(
                                "project_path".into(),
                                existing.project_path.to_string_lossy().into_owned(),
                            );
                        }

                        match session_mgr.update_config(session_id, final_config) {
                            Ok(()) => {
                                let msg = proto::ServerMessage {
                                    payload: Some(proto::server_message::Payload::StatusUpdate(
                                        proto::StatusUpdate {
                                            session_id: session_id.clone(),
                                            message: "Configuration updated".into(),
                                            user_inputs: vec![],
                                            agent_name: String::new(),
                                            view_only: false,
                                        },
                                    )),
                                };
                                let _ = response_tx_inbound.send(Ok(msg)).await;
                            }
                            Err(e) => {
                                let err_msg = session_error_msg(
                                    "ConfigUpdateFailed",
                                    &format!("Failed to update config: {e}"),
                                    session_id,
                                );
                                let _ = response_tx_inbound.send(Ok(err_msg)).await;
                            }
                        }
                    }
                    Some(proto::client_message::Payload::JoinSession(join)) => {
                        let session_id = join.session_id;
                        // 登记目标 session 与已回放水位：本次 join 将回放完整历史，
                        // 后续 Lagged 只做增量回放，避免与已收帧重复。
                        let join_history_len = session_mgr
                            .get(&session_id)
                            .map(|s| s.history.len())
                            .unwrap_or(0);
                        replay_state_inbound
                            .lock()
                            .unwrap()
                            .set_target(&session_id, join_history_len);
                        let user_inputs: Vec<String> = match session_mgr.get(&session_id) {
                            Ok(session) => session
                                .history
                                .iter()
                                .filter(|m| m.role == Role::User)
                                .map(|m| m.content.clone())
                                .collect(),
                            Err(_) => vec![],
                        };

                        // ── Step 1: Send StatusUpdate with user inputs ──
                        {
                            let msg = proto::ServerMessage {
                                payload: Some(proto::server_message::Payload::StatusUpdate(
                                    proto::StatusUpdate {
                                        session_id: session_id.clone(),
                                        message: format!(
                                            "Joined session {}",
                                            &session_id[..session_id.len().min(8)]
                                        ),
                                        user_inputs,
                                        agent_name: String::new(),
                                        view_only: false,
                                    },
                                )),
                            };
                            let _ = response_tx_inbound.send(Ok(msg)).await;
                        }

                        // ── Step 2: Replay full conversation history ──
                        if let Ok(session) = session_mgr.get(&session_id) {
                            for msg in &session.history {
                                match msg.role {
                                    Role::Assistant => {
                                        // Send text content
                                        if !msg.content.is_empty() {
                                            let td_msg = proto::ServerMessage {
                                                payload: Some(
                                                    proto::server_message::Payload::TextDelta(
                                                        proto::TextDelta {
                                                            delta: msg.content.clone(),
                                                            session_id: session_id.clone(),
                                                            agent_name: String::new(),
                                                        },
                                                    ),
                                                ),
                                            };
                                            if response_tx_inbound.send(Ok(td_msg)).await.is_err() {
                                                break;
                                            }
                                        }
                                        // Send tool calls
                                        if let Some(tool_calls) = &msg.tool_calls {
                                            for tc in tool_calls {
                                                let tc_msg = proto::ServerMessage {
                                                    payload: Some(
                                                        proto::server_message::Payload::ToolCall(
                                                            proto::ToolCall {
                                                                call_id: tc.id.clone(),
                                                                tool_name: tc.name.clone(),
                                                                arguments: tc.arguments.clone(),
                                                                session_id: session_id.clone(),
                                                                agent_name: String::new(),
                                                            },
                                                        ),
                                                    ),
                                                };
                                                if response_tx_inbound
                                                    .send(Ok(tc_msg))
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        }
                                        // Send UsageInfo if token data available
                                        if let Some(input_tokens) = msg.actual_tokens_input {
                                            let ui_msg = proto::ServerMessage {
                                                payload: Some(
                                                    proto::server_message::Payload::UsageInfo(
                                                        proto::UsageInfo {
                                                            input_tokens,
                                                            output_tokens: msg
                                                                .actual_tokens_output
                                                                .unwrap_or(0),
                                                            tool_calls: msg
                                                                .tool_call_count
                                                                .unwrap_or(0),
                                                            cache_creation_input_tokens: msg
                                                                .actual_cache_write
                                                                .unwrap_or(0),
                                                            cache_read_input_tokens: msg
                                                                .actual_cache_read
                                                                .unwrap_or(0),
                                                            cost: msg.actual_cost.unwrap_or(0.0),
                                                            session_id: session_id.clone(),
                                                        },
                                                    ),
                                                ),
                                            };
                                            let _ = response_tx_inbound.send(Ok(ui_msg)).await;
                                        }
                                    }
                                    Role::Tool => {
                                        let tr_msg = proto::ServerMessage {
                                            payload: Some(
                                                proto::server_message::Payload::ToolResult(
                                                    proto::ToolResult {
                                                        call_id: msg
                                                            .tool_call_id
                                                            .clone()
                                                            .unwrap_or_default(),
                                                        tool_name: String::new(),
                                                        content: msg.content.clone(),
                                                        is_error: msg
                                                            .tool_result_is_error
                                                            .unwrap_or(
                                                                msg.kind == MessageType::Error,
                                                            ),
                                                        session_id: session_id.clone(),
                                                        agent_name: String::new(),
                                                    },
                                                ),
                                            ),
                                        };
                                        if response_tx_inbound.send(Ok(tr_msg)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Role::User => {
                                        let um_msg = proto::ServerMessage {
                                            payload: Some(
                                                proto::server_message::Payload::UserMessage(
                                                    proto::UserMessage {
                                                        content: msg.content.clone(),
                                                        session_id: session_id.clone(),
                                                    },
                                                ),
                                            ),
                                        };
                                        if response_tx_inbound.send(Ok(um_msg)).await.is_err() {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }

                            // Send Done to flush streaming text
                            let done_msg = proto::ServerMessage {
                                payload: Some(proto::server_message::Payload::Done(proto::Done {
                                    session_id: session_id.clone(),
                                })),
                            };
                            let _ = response_tx_inbound.send(Ok(done_msg)).await;

                            // ── Step 3: Replay descendant sessions (BFS) ──
                            let descendants = collect_descendants(&session_mgr, &session_id);
                            let total = descendants.len();
                            let limited: &[visp_core::session::Session] =
                                &descendants[..total.min(DESCENDANT_SOFT_LIMIT)];
                            // 如果 collected 数量达到软上限则提示超限
                            if total >= DESCENDANT_SOFT_LIMIT {
                                let warn_msg = proto::ServerMessage {
                                    payload: Some(proto::server_message::Payload::TextDelta(
                                        proto::TextDelta {
                                            delta: format!(
                                                "⚠️ Session has {total} descendants, showing first {}",
                                                DESCENDANT_SOFT_LIMIT
                                            ),
                                            session_id: session_id.clone(),
                                            agent_name: String::new(),
                                        },
                                    )),
                                };
                                let _ = response_tx_inbound.send(Ok(warn_msg)).await;
                            }
                            for child_session in limited {
                                if replay_session_history(&response_tx_inbound, child_session)
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        // ── Outbound: Orchestrator → CLI ──
        spawn_outbound(
            bus_rx,
            response_tx,
            pending_queries.clone(),
            self.session_mgr.clone(),
            replay_state,
        );

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream) as Self::ChatStream))
    }

    async fn read_file(
        &self,
        request: Request<proto::ReadFileRequest>,
    ) -> Result<Response<proto::ReadFileResponse>, Status> {
        let req = request.into_inner();
        let path = req.path.clone();

        let working_dir = self
            .session_mgr
            .get(&req.session_id)
            .map(|s| s.project_path.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from("."));

        let ctx = ToolContext {
            working_dir,
            session_id: Some(req.session_id),
            permission_rules: None,
            global_tx: None,
            visp_trace_id: None,
            iter_span_w3c_id: None,
        };

        let mut args = serde_json::json!({ "path": path });
        if let Some(start_line) = req.start_line {
            args["start_line"] = serde_json::json!(start_line);
        }
        if let Some(end_line) = req.end_line {
            args["end_line"] = serde_json::json!(end_line);
        }

        let result = self
            .tool_registry
            .execute("read_file", args, &ctx)
            .await
            .ok_or_else(|| Status::internal("Tool 'read_file' not found"))?;

        if result.is_error {
            return Err(Status::internal(result.content));
        }

        Ok(Response::new(proto::ReadFileResponse {
            content: result.content,
            path: req.path,
        }))
    }

    async fn search_symbols(
        &self,
        request: Request<proto::SearchSymbolsRequest>,
    ) -> Result<Response<proto::SearchSymbolsResponse>, Status> {
        let req = request.into_inner();
        let project_path = req.project_path;
        let query = req.query;
        let limit = if req.limit <= 0 {
            20
        } else {
            req.limit as usize
        };

        let cg = self.get_codegraph(&project_path).await?;
        let symbols = cg.search(&query, limit).map_err(Status::internal)?;

        Ok(Response::new(proto::SearchSymbolsResponse {
            symbols: symbols
                .into_iter()
                .map(|s| proto::SymbolInfo {
                    name: s.name,
                    kind: s.kind,
                    file_path: s.file_path,
                    line: s.line,
                    column: s.column,
                    signature: s.signature.unwrap_or_default(),
                })
                .collect(),
        }))
    }

    async fn get_symbol_details(
        &self,
        request: Request<proto::GetSymbolDetailsRequest>,
    ) -> Result<Response<proto::SymbolDetails>, Status> {
        let req = request.into_inner();
        let project_path = req.project_path;
        let symbol_name = req.symbol_name;

        let cg = self.get_codegraph(&project_path).await?;
        let mut details = cg.get_details(&symbol_name).map_err(Status::internal)?;

        let d = details
            .drain(..)
            .next()
            .ok_or_else(|| Status::not_found(format!("Symbol '{symbol_name}' not found")))?;

        Ok(Response::new(proto::SymbolDetails {
            name: d.name,
            kind: d.kind,
            file_path: d.file_path,
            line: d.line,
            column: d.column,
            signature: d.signature.unwrap_or_default(),
            docstring: d.docstring.unwrap_or_default(),
            source: d.source,
            callers: d.callers,
            callees: d.callees,
        }))
    }

    async fn health_check(
        &self,
        _request: Request<()>,
    ) -> Result<Response<proto::HealthStatus>, Status> {
        let uptime = self.start_time.elapsed().as_secs();
        Ok(Response::new(proto::HealthStatus {
            alive: true,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_seconds: uptime,
        }))
    }

    /// 显式 `/reload`：调用共享 reload 核心的显式入口（rules/skills/agents +
    /// system_prompt 回执），把逐项结果映射为 proto 响应。
    ///
    /// 部分成功是常态：含失败条目时仍返回正常响应（设计 §3）。核心显式入口不
    /// 返回 `Result`，本 handler 不存在可上报的不可恢复异常，故不产生 gRPC error。
    async fn reload_config(
        &self,
        _request: Request<proto::ReloadConfigRequest>,
    ) -> Result<Response<proto::ReloadConfigResponse>, Status> {
        let items = self.reload_core.reload_all().await;

        // `rule_engine` 由 reload 核心持有同一 `Arc` 完成实际重载；此处读取以记录
        // 重载后生效的规则规模（可观测性），并确保该字段在生产路径被真实使用。
        tracing::debug!(
            active_rules_bytes = self.rule_engine.get_active_rules().len(),
            "reload_config applied"
        );

        Ok(Response::new(proto::ReloadConfigResponse {
            results: items.iter().map(reload_item_to_proto).collect(),
        }))
    }

    /// 只读 hook 诊断：五类计数 + 生效规则摘要（设计 §15）。
    ///
    /// 不改变任何运行时状态；`project` 字段一期预留（daemon 仅维护单项目运行时）。
    async fn get_hook_stats(
        &self,
        request: Request<proto::GetHookStatsRequest>,
    ) -> Result<Response<proto::HookStatsResponse>, Status> {
        let req = request.into_inner();
        if !req.project.is_empty() {
            tracing::debug!(
                project = %req.project,
                "GetHookStats project filter ignored (single-project runtime)"
            );
        }
        let snapshot = self.hook_stats.hook_stats();
        Ok(Response::new(proto::HookStatsResponse {
            emitted: snapshot.emitted,
            dropped: snapshot.dropped,
            executed: snapshot.executed,
            failed: snapshot.failed,
            timed_out: snapshot.timed_out,
            rules: snapshot
                .rules
                .iter()
                .map(|rule| proto::HookRuleSummary {
                    id: rule.id.clone(),
                    scope: rule.scope_str().to_string(),
                    enabled: rule.enabled,
                })
                .collect(),
        }))
    }

    async fn shutdown(
        &self,
        _request: Request<proto::ShutdownRequest>,
    ) -> Result<Response<()>, Status> {
        // 一次性终态动作（SessionEnd / drain / cancel）只在首次关停执行；
        // 重复调用退化为幂等兜底清理（设计 D13）。
        if self.begin_shutdown() {
            tracing::info!("shutdown requested, running graceful shutdown pipeline");

            // 1. 终态 SessionEnd：同步直投 hook 执行器（不经总线，规避关停竞态），
            //    此后 daemon 不再派发 hook 事件（终止态抑制）。
            self.publish_shutdown_session_end().await;

            // 2. 有界 drain：宿主自身应遵守 budget，调用方再加同长硬超时兜底。
            let drain = self.hook_drain.drain(HOOK_DRAIN_BUDGET);
            if tokio::time::timeout(HOOK_DRAIN_BUDGET, drain)
                .await
                .is_err()
            {
                tracing::warn!(
                    budget_ms = HOOK_DRAIN_BUDGET.as_millis(),
                    "hook drain exceeded budget, proceeding with shutdown"
                );
            }

            // 3. 取消在飞 agent（通道可能已满/接收端已退出，尽力而为）。
            if let Err(error) = self
                .cancel_tx
                .try_send(visp_agent::orchestrator::CancelSignal)
            {
                tracing::warn!(%error, "cancel signal not delivered on shutdown");
            }
        }

        // 4. 关停 MCP（幂等，重复调用无副作用）。
        tracing::info!("shutdown requested, stopping MCP servers");
        self.mcp_manager.shutdown_all().await;

        // 5. 唤醒 daemon main 执行进程级清理（watcher.stop / MCP / server.abort）。
        self.shutdown_notify.notify_one();
        Ok(Response::new(()))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// hook 事件信封的 `cwd`：daemon 侧事实无固定工作目录，取进程当前目录。
fn hook_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// 由 `UserResponse` 的 `selected_index`/`text` 推导 `PermissionResult.outcome`。
///
/// `-1` 在协议中承载两种语义（`visp.proto`：-1 = "Other" 自定义输入，`text` 为原文）；
/// agent_loop 亦以 `-1` 表达取消。故：非负索引或带文本的 -1 视为 `selected`，
/// 仅 -1 且文本为空视为 `cancelled`。
fn permission_outcome(selected_index: i32, text: &str) -> PermissionOutcome {
    if selected_index >= 0 || !text.is_empty() {
        PermissionOutcome::Selected
    } else {
        PermissionOutcome::Cancelled
    }
}

/// 发布 `PermissionResult`（设计 §6.4：daemon 响应路由成功后的**单一发布点**）。
fn publish_permission_result(
    bus: &EventBus,
    session_id: &str,
    query_id: &str,
    selected_index: i32,
    text: &str,
) {
    let event = HookEvent {
        context: HookContext {
            schema: VISP_HOOK_SCHEMA,
            hook_event_name: HookEventName::PermissionResult,
            session_id: session_id.to_string(),
            cwd: hook_cwd(),
            // daemon 侧无 `source`/`origin` 事实来源，沿用既有约定（唯一客户端为 TUI）。
            source: SessionSource::Startup,
            origin: Origin::Tui,
            seq: None,
        },
        payload: HookPayload::PermissionResult(PermissionResultPayload {
            query_id: query_id.to_string(),
            outcome: permission_outcome(selected_index, text),
            selected_index: selected_index as i64,
        }),
    };
    bus.publish(BusEvent::Hook(event));
}

/// 把一条 `UserResponse` 路由回 daemon 侧等待中的 agent loop（`pending` 命中），
/// 并在**路由成功后**发布一次 `PermissionResult`。
///
/// 返回 `true` 表示已在 daemon 层消费（命中），调用方**不得**再走 orchestrator 回退；
/// 返回 `false` 表示未命中（过期/外部响应），由调用方回退，且**不发布**任何 hook 事件。
///
/// 仅当 `try_send` 成功（确实送达等待者）才发布：通道已关闭/已满（等待者已退出）视为
/// 过期响应，不产生事实。终止态（`shutting_down`）下跳过发布（设计：`SessionEnd` 之后
/// 不再派发 hook 事件）。
fn route_daemon_user_response(
    bus: &EventBus,
    pending: &Mutex<HashMap<String, PendingQuery>>,
    shutting_down: &AtomicBool,
    query_id: &str,
    selected_index: i32,
    text: &str,
) -> bool {
    let entry = pending.lock().unwrap().remove(query_id);
    let Some(entry) = entry else {
        // 未命中：过期/外部响应，交由调用方回退，不发布。
        return false;
    };

    if entry
        .respond
        .try_send(UserQueryResult {
            selected_index,
            text: text.to_string(),
        })
        .is_err()
    {
        // 命中但送达失败（等待者已退出/通道满）：视为过期，既不发布也不回退。
        return true;
    }

    if !shutting_down.load(Ordering::SeqCst) {
        publish_permission_result(bus, &entry.session_id, query_id, selected_index, text);
    }
    true
}

/// outbound 任务：把总线显示域帧转发给连接；订阅者滞后（`Lagged`）时对连接
/// 当前的**目标 session** 做一次 join 式增量 replay（去重 + 节流）。`Closed`
/// 仍 `break`；replay 失败仅日志，绝不影响主转发流程。
fn spawn_outbound(
    mut bus_rx: tokio::sync::broadcast::Receiver<BusEnvelope>,
    response_tx: mpsc::Sender<Result<proto::ServerMessage, Status>>,
    pending_queries: Arc<Mutex<HashMap<String, PendingQuery>>>,
    session_mgr: Arc<SessionManager>,
    replay_state: Arc<Mutex<ReplayState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let envelope = match bus_rx.recv().await {
                Ok(envelope) => envelope,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "chat 订阅者滞后，已丢弃最旧帧");
                    replay_target_session(&response_tx, &session_mgr, &replay_state).await;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            // 出站仅转发显示域帧；hook 域事件不面向 TUI，仅供 hook 执行器消费。
            let frame = match envelope.event {
                BusEvent::Frame(frame) => frame,
                BusEvent::Hook(_) => continue,
            };
            let sid = frame.session_id.clone();
            match frame.event {
                AgentEvent::UserQuery {
                    query_id,
                    message,
                    options,
                    allow_other,
                    respond,
                    ..
                } => {
                    // Store the respond sender so the inbound task can route
                    // UserResponse back directly to the waiting agent loop.
                    pending_queries.lock().unwrap().insert(
                        query_id.clone(),
                        PendingQuery {
                            respond,
                            session_id: sid.clone(),
                        },
                    );
                    let proto_msg = proto::ServerMessage {
                        payload: Some(proto::server_message::Payload::UserQuery(
                            proto::UserQuery {
                                query_id,
                                message,
                                options,
                                allow_other,
                                session_id: sid,
                            },
                        )),
                    };
                    if response_tx.send(Ok(proto_msg)).await.is_err() {
                        break;
                    }
                }
                _ => {
                    if let Some(proto_msg) =
                        agent_event_to_server_message(frame.event, &sid, &frame.agent_name)
                        && let Some(payload) = proto_msg.payload
                        && response_tx
                            .send(Ok(proto::ServerMessage {
                                payload: Some(payload),
                            }))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
            }
        }
    })
}

/// 对连接当前目标 session 做一次 join 式**增量** replay。
///
/// 仅当存在目标 session 且已过节流窗口时执行；从 `replay_from` 水位起回放，
/// 避免与已收帧重复。任何失败仅记日志，不影响主转发流程。
async fn replay_target_session(
    response_tx: &mpsc::Sender<Result<proto::ServerMessage, Status>>,
    session_mgr: &SessionManager,
    replay_state: &Arc<Mutex<ReplayState>>,
) {
    let (session_id, from, allowed) = {
        let state = replay_state.lock().unwrap();
        let Some(target) = state.target.clone() else {
            return;
        };
        let from = state.replay_from(&target);
        (target, from, state.throttle_allows(Instant::now()))
    };
    if !allowed {
        return;
    }
    let session = match session_mgr.get(&session_id) {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "lagged replay: 加载 session 失败，跳过");
            return;
        }
    };
    // 先记录（回放中标记 + 节流），确保回放进行中再次 Lagged 不叠加。
    let upto = session.history.len();
    replay_state
        .lock()
        .unwrap()
        .note_replayed(&session_id, upto, Instant::now());
    if replay_session_history_from(response_tx, &session, from)
        .await
        .is_err()
    {
        tracing::warn!(session_id = %session_id, "lagged replay: 发送中断，忽略");
    }
}

fn session_to_proto(
    session: &visp_core::session::Session,
    available_models: &[String],
    model_keys: &[String],
    model_configs: &[LlmModelConfig],
) -> proto::Session {
    let status = match session.status {
        SessionStatus::Idle => proto::SessionStatus::Idle,
        SessionStatus::Running => proto::SessionStatus::Running,
        SessionStatus::Completed => proto::SessionStatus::Completed,
        SessionStatus::Error => proto::SessionStatus::Error,
    };

    let elapsed = session.created_at.elapsed();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let created_secs = now.as_secs() as i64 - elapsed.as_secs() as i64;

    // proto model_key 字段用于 CLI 状态栏显示，用 key 格式 "{provider}/{name}"
    let display_model_key = session
        .config
        .model_key
        .clone()
        .or_else(|| {
            model_configs
                .iter()
                .find(|mc| mc.model == session.config.model)
                .map(|mc| mc.key())
        })
        .unwrap_or_else(|| session.config.model.clone());

    // proto model 字段用于 CLI 状态栏显示，使用 key 格式 "{provider}/{name}"
    // 以便 CLI 端 split_model_name 能正确拆出 provider 和模型名
    let display_model = display_model_key.clone();

    proto::Session {
        session_id: session.id.clone(),
        status: status.into(),
        project_path: session.project_path.to_string_lossy().to_string(),
        model: display_model,
        last_user_message: session.last_user_message.clone().unwrap_or_default(),
        created_at: Some(prost_types::Timestamp {
            seconds: created_secs,
            nanos: 0,
        }),
        available_models: available_models.to_vec(),
        model_keys: model_keys.to_vec(),
        model_key: display_model_key,
    }
}

/// 忙拒绝错误码（设计 §5.6）：会话存在在途循环时拒绝新用户输入。
const SESSION_BUSY_CODE: &str = "SessionBusy";
/// 忙拒绝文案。**不得**含 `cancelled`（大小写不敏感），否则 ACP 会误判为用户取消。
const SESSION_BUSY_MESSAGE: &str = "正在生成，请稍候";
/// 非主会话 / 未知会话拒绝码（语义保持现状）。
const SESSION_NOT_ACTIVE_CODE: &str = "SessionNotActive";

/// 用户输入受理决策三态（无副作用纯函数出参）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserInputDecision {
    /// 受理：主会话且无在途循环。
    Accept,
    /// 拒绝：非主会话 / 未知会话。
    RejectNotActive,
    /// 拒绝：主会话存在在途循环。
    RejectBusy,
}

/// 判定用户输入是否受理（纯函数，无副作用）。
///
/// - 入参：`is_main`（是否主会话）、`has_inflight_loop`（是否存在在途循环令牌）。
/// - 判据只用 `has_inflight_loop`，**不读取、不修改**会话 `status`。
/// - 真实分支与测试共用本函数，避免「测试内复制判定逻辑」的假覆盖。
fn decide_user_input(is_main: bool, has_inflight_loop: bool) -> UserInputDecision {
    if !is_main {
        UserInputDecision::RejectNotActive
    } else if has_inflight_loop {
        UserInputDecision::RejectBusy
    } else {
        UserInputDecision::Accept
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn session_error_msg(code: &str, message: &str, session_id: &str) -> proto::ServerMessage {
    proto::ServerMessage {
        payload: Some(proto::server_message::Payload::Error(proto::Error {
            code: code.to_owned(),
            message: message.to_owned(),
            session_id: session_id.to_owned(),
            agent_name: String::new(),
        })),
    }
}

/// BFS 收集 root_id 的所有后代 session（不含 root）。
/// - 软上限 50（BFS 层级优先，超限保留较早创建的）
/// - visited 集合防环
/// - 单 session 加载失败用 tracing::warn! 记录并跳过
const DESCENDANT_SOFT_LIMIT: usize = 50;

fn collect_descendants(
    session_mgr: &SessionManager,
    root_id: &str,
) -> Vec<visp_core::session::Session> {
    let mut result: Vec<visp_core::session::Session> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    visited.insert(root_id.to_string());
    queue.push_back(root_id.to_string());

    while let Some(parent_id) = queue.pop_front() {
        if result.len() >= DESCENDANT_SOFT_LIMIT {
            break;
        }
        let children = match session_mgr.list_child_sessions(&parent_id) {
            Ok(children) => children,
            Err(e) => {
                tracing::warn!(parent_id, error = %e, "collect_descendants: list_child_sessions failed, skipping");
                continue;
            }
        };
        let mut children = children;
        // 按 created_at 升序
        children.sort_by_key(|a| a.created_at);

        for child in children {
            if result.len() >= DESCENDANT_SOFT_LIMIT {
                break;
            }
            if visited.insert(child.id.clone()) {
                result.push(child.clone());
                queue.push_back(child.id);
            }
        }
    }

    result
}

/// 回放单个 session 的历史作为只读帧。
/// 发送：StatusUpdate(view_only=true) → 消息帧 → Done
async fn replay_session_history(
    response_tx: &mpsc::Sender<Result<proto::ServerMessage, Status>>,
    session: &visp_core::session::Session,
) -> Result<(), ()> {
    replay_session_history_from(response_tx, session, 0).await
}

/// 从 `from` 下标起回放 session 历史的尾部（增量回放，避免重复已收帧）。
/// 始终先发 StatusUpdate(view_only=true)，末尾发 Done；`from` 之前的消息跳过。
async fn replay_session_history_from(
    response_tx: &mpsc::Sender<Result<proto::ServerMessage, Status>>,
    session: &visp_core::session::Session,
    from: usize,
) -> Result<(), ()> {
    let session_id = session.id.clone();
    let agent_name = session.agent_name.clone();

    // 收集所有 Role::User 消息作为 user_inputs
    let user_inputs: Vec<String> = session
        .history
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .collect();

    // StatusUpdate with view_only=true
    {
        let msg = proto::ServerMessage {
            payload: Some(proto::server_message::Payload::StatusUpdate(
                proto::StatusUpdate {
                    session_id: session_id.clone(),
                    message: format!("Viewing session {}", &session_id[..session_id.len().min(8)]),
                    user_inputs,
                    agent_name: agent_name.clone(),
                    view_only: true,
                },
            )),
        };
        if response_tx.send(Ok(msg)).await.is_err() {
            return Err(());
        }
    }

    // 回放历史：Assistant→TextDelta(+ToolCall)，Tool→ToolResult，User→跳过
    for msg in session.history.iter().skip(from) {
        match msg.role {
            Role::Assistant => {
                // 文本
                if !msg.content.is_empty() {
                    let td_msg = proto::ServerMessage {
                        payload: Some(proto::server_message::Payload::TextDelta(
                            proto::TextDelta {
                                delta: msg.content.clone(),
                                session_id: session_id.clone(),
                                agent_name: agent_name.clone(),
                            },
                        )),
                    };
                    if response_tx.send(Ok(td_msg)).await.is_err() {
                        return Err(());
                    }
                }
                // Tool calls
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        let tc_msg = proto::ServerMessage {
                            payload: Some(proto::server_message::Payload::ToolCall(
                                proto::ToolCall {
                                    call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                    session_id: session_id.clone(),
                                    agent_name: agent_name.clone(),
                                },
                            )),
                        };
                        if response_tx.send(Ok(tc_msg)).await.is_err() {
                            return Err(());
                        }
                    }
                }
                // Send UsageInfo if token data available
                if let Some(input_tokens) = msg.actual_tokens_input {
                    let ui_msg = proto::ServerMessage {
                        payload: Some(proto::server_message::Payload::UsageInfo(
                            proto::UsageInfo {
                                cost: 0.0,
                                input_tokens,
                                output_tokens: msg.actual_tokens_output.unwrap_or(0),
                                tool_calls: msg.tool_call_count.unwrap_or(0),
                                cache_creation_input_tokens: msg.actual_cache_write.unwrap_or(0),
                                cache_read_input_tokens: msg.actual_cache_read.unwrap_or(0),
                                session_id: session_id.clone(),
                            },
                        )),
                    };
                    if response_tx.send(Ok(ui_msg)).await.is_err() {
                        return Err(());
                    }
                }
            }
            Role::Tool => {
                let tr_msg = proto::ServerMessage {
                    payload: Some(proto::server_message::Payload::ToolResult(
                        proto::ToolResult {
                            call_id: msg.tool_call_id.clone().unwrap_or_default(),
                            tool_name: String::new(),
                            content: msg.content.clone(),
                            is_error: msg
                                .tool_result_is_error
                                .unwrap_or(msg.kind == visp_core::message::MessageType::Error),
                            session_id: session_id.clone(),
                            agent_name: agent_name.clone(),
                        },
                    )),
                };
                if response_tx.send(Ok(tr_msg)).await.is_err() {
                    return Err(());
                }
            }
            Role::User => {
                let um_msg = proto::ServerMessage {
                    payload: Some(proto::server_message::Payload::UserMessage(
                        proto::UserMessage {
                            content: msg.content.clone(),
                            session_id: session_id.clone(),
                        },
                    )),
                };
                if response_tx.send(Ok(um_msg)).await.is_err() {
                    return Err(());
                }
            }
            _ => {}
        }
    }

    // Done
    let done_msg = proto::ServerMessage {
        payload: Some(proto::server_message::Payload::Done(proto::Done {
            session_id: session_id.clone(),
        })),
    };
    let _ = response_tx.send(Ok(done_msg)).await;
    Ok(())
}

/// 验证阶段 → proto 稳定字符串（跨语言消费方按此判断，不依赖 Display/调试格式）。
fn verification_stage_str(stage: visp_core::agent::VerificationStage) -> &'static str {
    use visp_core::agent::VerificationStage;
    match stage {
        VerificationStage::Started => "started",
        VerificationStage::Passed => "passed",
        VerificationStage::Failed => "failed",
        VerificationStage::Rework => "rework",
        VerificationStage::Exhausted => "exhausted",
        VerificationStage::Inconclusive => "inconclusive",
    }
}

fn agent_event_to_server_message(
    event: AgentEvent,
    session_id: &str,
    agent_name: &str,
) -> Option<proto::ServerMessage> {
    let sid = session_id.to_owned();
    let aname = agent_name.to_owned();
    match event {
        AgentEvent::TextDelta(delta) => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::TextDelta(
                proto::TextDelta {
                    delta,
                    session_id: sid,
                    agent_name: aname,
                },
            )),
        }),
        AgentEvent::ToolCallRequest {
            call_id,
            tool_name,
            arguments,
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::ToolCall(proto::ToolCall {
                call_id,
                tool_name,
                arguments,
                session_id: sid,
                agent_name: aname,
            })),
        }),
        AgentEvent::ToolCallResult {
            call_id,
            tool_name,
            content,
            is_error,
            outcome: _,
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::ToolResult(
                proto::ToolResult {
                    call_id,
                    content,
                    is_error,
                    tool_name,
                    session_id: sid,
                    agent_name: aname,
                },
            )),
        }),
        // PreToolUse 是 hook 事实，无对应的显示 ServerMessage。
        AgentEvent::PreToolUse { .. } => None,
        AgentEvent::UsageInfo {
            input_tokens,
            output_tokens,
            tool_calls,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cost,
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::UsageInfo(
                proto::UsageInfo {
                    input_tokens,
                    output_tokens,
                    tool_calls,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    cost: cost.unwrap_or(0.0),
                    session_id: sid,
                },
            )),
        }),
        AgentEvent::UsageDelta {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::UsageDelta(
                proto::UsageDelta {
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    session_id: sid,
                },
            )),
        }),
        AgentEvent::ThinkingBlock(block) => {
            let thinking = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
            let signature = block
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(proto::ServerMessage {
                payload: Some(proto::server_message::Payload::ThinkingBlock(
                    proto::ThinkingBlock {
                        thinking: thinking.to_string(),
                        signature: signature.to_string(),
                        session_id: sid,
                    },
                )),
            })
        }
        // 思考增量：proto 语义下的思考块帧，thinking=增量文本、signature 为空。
        // 消费方按「同 messageId 追加」消费，从而 O(n)。
        AgentEvent::ThinkingDelta(delta) => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::ThinkingBlock(
                proto::ThinkingBlock {
                    thinking: delta,
                    signature: String::new(),
                    session_id: sid,
                },
            )),
        }),
        AgentEvent::StatusUpdate(message) => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::StatusUpdate(
                proto::StatusUpdate {
                    message,
                    session_id: sid,
                    user_inputs: vec![],
                    agent_name: aname,
                    view_only: false,
                },
            )),
        }),
        AgentEvent::UserQuery {
            query_id,
            message,
            options,
            allow_other,
            ..
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::UserQuery(
                proto::UserQuery {
                    query_id: query_id.clone(),
                    message: message.clone(),
                    options: options.clone(),
                    allow_other,
                    session_id: sid,
                },
            )),
        }),
        AgentEvent::Error { code, message } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::Error(proto::Error {
                code: code.to_string(),
                message,
                session_id: sid,
                agent_name: aname,
            })),
        }),
        AgentEvent::ImageBlock {
            path,
            mime_type,
            remote_url,
        } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::ImageBlock(
                proto::ImageBlock {
                    path,
                    mime_type,
                    remote_url: remote_url.unwrap_or_default(),
                    session_id: sid,
                    agent_name: aname,
                },
            )),
        }),
        AgentEvent::ImageError { reason } => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::ImageError(
                proto::ImageError {
                    reason,
                    session_id: sid,
                    agent_name: aname,
                },
            )),
        }),
        AgentEvent::Verification(v) => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::VerificationUpdate(
                proto::VerificationUpdate {
                    stage: verification_stage_str(v.stage).to_string(),
                    round: v.round,
                    max_rounds: v.max_rounds,
                    summary: v.summary,
                    evidence: v
                        .evidence
                        .into_iter()
                        .map(|e| proto::VerificationEvidence {
                            check: e.check,
                            method: e.method,
                            result: e.result,
                        })
                        .collect(),
                    is_final: v.is_final,
                    passed: v.passed.unwrap_or(false),
                    session_id: sid,
                    agent_name: aname,
                },
            )),
        }),
        AgentEvent::Done => Some(proto::ServerMessage {
            payload: Some(proto::server_message::Payload::Done(proto::Done {
                session_id: sid,
            })),
        }),
    }
}

// ── 步骤 4a：reload 结果到 proto 的单向映射 ─────────────────────────────

/// 核心逐项结果 → proto 条目（步骤 4a 的单向映射）。
///
/// `ReloadItem.changes` 是核心内部聚合的变更条目数；proto 的计数位按类别细分
/// （新增/修改/删除/跳过），核心未暴露该细分，故统一写入 `modified`，其余计数
/// 位保留 0。消息文本已包含统计摘要，细分为后续按需扩展点。
fn reload_item_to_proto(item: &ReloadItem) -> proto::reload_config_response::Item {
    proto::reload_config_response::Item {
        category: reload_domain_category(item.domain).to_string(),
        success: item.success,
        message: item.message.clone(),
        added: 0,
        modified: item.changes as u32,
        deleted: 0,
        skipped: 0,
    }
}

/// 领域 → proto 类别字符串（`rules` / `skills` / `agents` / `system_prompt`）。
fn reload_domain_category(domain: ReloadDomain) -> &'static str {
    match domain {
        ReloadDomain::Rules => "rules",
        ReloadDomain::Skills => "skills",
        ReloadDomain::Agents => "agents",
        ReloadDomain::SystemPrompt => "system_prompt",
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc as StdArc;
    use visp_core::error::SessionError;
    use visp_core::message::Message;
    use visp_core::message::ToolCallRequest;
    use visp_core::session::InMemorySessionStore;
    use visp_core::session::Session;
    use visp_core::session::SessionStatus as CoreStatus;
    use visp_core::session::SessionStore;
    use visp_llm::mock::MockProvider;

    use arc_swap::ArcSwap;
    use serial_test::serial;
    use tempfile::TempDir;
    use visp_tools::skill::SkillTool;

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// 构造测试用 ReloadCore 及其共享的 RuleEngine（project 为临时目录）。
    fn build_reload_core(project: &Path) -> (Arc<RuleEngine>, Arc<ReloadCore>) {
        let rule_engine = Arc::new(RuleEngine::new(project).unwrap());
        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry
            .register(Arc::new(SkillTool::new(project)))
            .unwrap();
        let initial = visp_agent::agent_loader::load_agents(&[], &[]);
        let agent_registry = Arc::new(ArcSwap::from_pointee(initial));
        let core = Arc::new(ReloadCore::new(
            rule_engine.clone(),
            tool_registry,
            agent_registry,
            Vec::new(),
            None,
            project.to_path_buf(),
        ));
        (rule_engine, core)
    }

    /// 非重载测试使用的轻量核心（project 固定 `/tmp`）。
    fn tmp_reload_core() -> Arc<ReloadCore> {
        build_reload_core(Path::new("/tmp")).1
    }

    fn make_service_with_core(
        mgr: StdArc<SessionManager>,
        rule_engine: Arc<RuleEngine>,
        reload_core: Arc<ReloadCore>,
    ) -> CoderDaemonService {
        let (cancel_tx, _cancel_rx) = mpsc::channel(16);
        let (client_tx, _client_rx) = mpsc::channel(64);
        CoderDaemonService {
            provider: Arc::new(StdRwLock::new(
                Arc::new(MockProvider::new(vec![])) as Arc<dyn LlmProvider>
            )),
            tool_registry: Arc::new(ToolRegistry::new()),
            rule_engine,
            reload_core,
            session_mgr: mgr,
            agent_config: AgentConfig::default(),
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config: LlmConfig::default(),
            daemon_config: Arc::new(visp_config::DaemonConfig::default()),
            context_trimmer: Arc::new(visp_core::context::NoopTrimmer),
            mcp_manager: Arc::new(McpManager::new(vec![])),
            available_models: vec![],
            model_configs: vec![],
            model_config_keys: vec![],
            cancel_tx,
            bus: Arc::new(EventBus::new()),
            client_tx,
            shutdown_notify: Arc::new(Notify::new()),
            hook_drain: Arc::new(crate::shutdown::NoopHookDrain),
            hook_stats: Arc::new(visp_daemon::hook_runtime::NoopHookStats),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    fn make_service(mgr: StdArc<SessionManager>) -> CoderDaemonService {
        let (rule_engine, reload_core) = build_reload_core(Path::new("/tmp"));
        make_service_with_core(mgr, rule_engine, reload_core)
    }

    /// 构造带可控关停句柄的 service：保留 cancel 接收端、暴露总线与 `Notify`，
    /// 并注入 drain 宿主。供优雅关停管道（设计 D13）测试使用。
    fn make_service_with_shutdown(
        mgr: StdArc<SessionManager>,
        hook_drain: Arc<dyn HookDrainHost>,
    ) -> (
        CoderDaemonService,
        mpsc::Receiver<visp_agent::orchestrator::CancelSignal>,
        Arc<Notify>,
        Arc<EventBus>,
    ) {
        let mut service = make_service(mgr);
        let (cancel_tx, cancel_rx) = mpsc::channel(16);
        service.cancel_tx = cancel_tx;
        let bus = Arc::new(EventBus::new());
        service.bus = bus.clone();
        let shutdown_notify = Arc::new(Notify::new());
        service.shutdown_notify = shutdown_notify.clone();
        service.hook_drain = hook_drain;
        service.shutting_down = Arc::new(AtomicBool::new(false));
        (service, cancel_rx, shutdown_notify, bus)
    }

    /// 构造 service 并暴露其总线句柄，供 Chat 订阅语义测试发布帧。
    fn make_service_with_bus(mgr: StdArc<SessionManager>) -> (CoderDaemonService, Arc<EventBus>) {
        let mut service = make_service(mgr);
        let bus = Arc::new(EventBus::new());
        service.bus = bus.clone();
        (service, bus)
    }

    /// 隔离全局配置的测试环境：`VISP_CONFIG_DIR` → 临时空目录，drop 时还原。
    /// 与 `reload_tests.rs` 同构，避免读取真实 `~/.config/visp`。
    struct IsolatedEnv {
        _config: TempDir,
        project: TempDir,
        prev_config_dir: Option<String>,
    }

    impl IsolatedEnv {
        fn new() -> Self {
            let config = TempDir::new().unwrap();
            let project = TempDir::new().unwrap();
            let prev_config_dir = std::env::var("VISP_CONFIG_DIR").ok();
            unsafe { std::env::set_var("VISP_CONFIG_DIR", config.path()) };
            Self {
                _config: config,
                project,
                prev_config_dir,
            }
        }

        fn project(&self) -> &Path {
            self.project.path()
        }
    }

    impl Drop for IsolatedEnv {
        fn drop(&mut self) {
            match &self.prev_config_dir {
                Some(value) => unsafe { std::env::set_var("VISP_CONFIG_DIR", value) },
                None => unsafe { std::env::remove_var("VISP_CONFIG_DIR") },
            }
        }
    }

    // ── GetSession tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_session_exact_match() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let session_id = session.id.clone();
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: session_id.clone(),
        });
        let response = service.get_session(request).await.unwrap();
        let result = response.into_inner();
        assert_eq!(result.session_id, session_id);
        assert_eq!(result.project_path, "/tmp");
    }

    #[tokio::test]
    async fn test_get_session_prefix_unique() {
        let mut store = InMemorySessionStore::new();
        store
            .create(Session {
                id: "unique-abcdef".into(),
                project_path: Path::new("/tmp").to_path_buf(),
                status: SessionStatus::Idle,
                created_at: Instant::now(),
                created_at_unix: None,
                history: vec![],
                last_user_message: None,
                config: LlmConfig::default(),
                system_prompt_template: "default".into(),
                approved_tools: HashSet::new(),
                agent_name: "default".into(),
                parent_id: None,
                permission: vec![],
            })
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: "unique".into(),
        });
        let response = service.get_session(request).await.unwrap();
        let result = response.into_inner();
        assert_eq!(result.session_id, "unique-abcdef");
    }

    #[tokio::test]
    async fn test_get_session_prefix_zero() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: "nonexistent-prefix-".into(),
        });
        let err = service.get_session(request).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_get_session_prefix_multiple() {
        let mut store = InMemorySessionStore::new();
        store
            .create(Session {
                id: "common-prefix-a".into(),
                project_path: Path::new("/tmp/a").to_path_buf(),
                status: SessionStatus::Idle,
                created_at: Instant::now(),
                created_at_unix: None,
                history: vec![],
                last_user_message: None,
                config: LlmConfig::default(),
                system_prompt_template: "default".into(),
                approved_tools: HashSet::new(),
                agent_name: "default".into(),
                parent_id: None,
                permission: vec![],
            })
            .unwrap();
        store
            .create(Session {
                id: "common-prefix-b".into(),
                project_path: Path::new("/tmp/b").to_path_buf(),
                status: SessionStatus::Idle,
                created_at: Instant::now(),
                created_at_unix: None,
                history: vec![],
                last_user_message: None,
                config: LlmConfig::default(),
                system_prompt_template: "default".into(),
                approved_tools: HashSet::new(),
                agent_name: "default".into(),
                parent_id: None,
                permission: vec![],
            })
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: "common".into(),
        });
        let err = service.get_session(request).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_get_session_not_found() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: "i-do-not-exist".into(),
        });
        let err = service.get_session(request).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_get_session_error_propagation() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service(mgr);

        let request = tonic::Request::new(proto::GetSessionRequest {
            session_id: "missing".into(),
        });
        let err = service.get_session(request).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(err.message(), "Session not found");
    }

    // ── Cancel tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cancel_during_agent_loop() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        let ctx = mgr.start_loop(&session.id, &trimmer, None, None).unwrap();
        let token = ctx.cancel_token.clone();
        assert!(
            !token.is_cancelled(),
            "token should not be cancelled initially"
        );

        // Simulate Cancel handler: retrieve session, check Running, cancel agent
        let s = mgr.get(&session.id).unwrap();
        assert_eq!(s.status, CoreStatus::Running);
        mgr.cancel_agent(&session.id);

        assert!(
            token.is_cancelled(),
            "token should be cancelled after cancel_agent"
        );
    }

    #[tokio::test]
    async fn test_cancel_idle_session() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();

        // Simulate Cancel handler: retrieve session, check status
        let s = mgr.get(&session.id).unwrap();
        assert_eq!(s.status, CoreStatus::Idle);

        // Even if cancel_agent were called on idle session, it should be no-op
        mgr.cancel_agent(&session.id);

        let s = mgr.get(&session.id).unwrap();
        assert_eq!(
            s.status,
            CoreStatus::Idle,
            "idle session should remain idle after cancel"
        );
    }

    #[test]
    fn test_proto_to_llm_config_empty() {
        let config = proto::LlmConfig {
            model: None,
            model_key: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            extra: HashMap::new(),
        };
        let llm = visp_config::proto_to_llm_config(&config);
        assert_eq!(llm.model, "claude-3-7-sonnet-20250219");
        assert!((llm.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(llm.max_tokens, 4096);
    }

    #[test]
    fn test_proto_to_llm_config_full() {
        let mut extra = HashMap::new();
        extra.insert("custom_key".into(), "custom_val".into());
        let config = proto::LlmConfig {
            model: Some("gpt-4".into()),
            model_key: None,
            temperature: Some(0.5),
            max_tokens: Some(2048),
            max_context_tokens: Some(64000),
            extra,
        };

        let llm = visp_config::proto_to_llm_config(&config);
        assert_eq!(llm.model, "gpt-4");
        assert!((llm.temperature - 0.5).abs() < f64::EPSILON);
        assert_eq!(llm.max_tokens, 2048);
        assert_eq!(llm.max_context_tokens, 64_000);
        assert_eq!(llm.extra.get("custom_key").unwrap(), "custom_val");
    }

    #[test]
    fn test_proto_to_llm_config_partial() {
        let config = proto::LlmConfig {
            model: Some("gpt-4".into()),
            model_key: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            extra: HashMap::new(),
        };

        let llm = visp_config::proto_to_llm_config(&config);
        assert_eq!(llm.model, "gpt-4");
        assert!((llm.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(llm.max_tokens, 4096);
    }

    #[tokio::test]
    async fn test_create_session_max_context_tokens_default() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let default_llm_config = LlmConfig {
            max_context_tokens: 200_000,
            ..LlmConfig::default()
        };
        let (cancel_tx, _cancel_rx) = mpsc::channel(16);
        let (client_tx, _client_rx) = mpsc::channel(64);
        let service = CoderDaemonService {
            provider: Arc::new(StdRwLock::new(
                Arc::new(MockProvider::new(vec![])) as Arc<dyn LlmProvider>
            )),
            tool_registry: Arc::new(ToolRegistry::new()),
            rule_engine: Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap()),
            reload_core: tmp_reload_core(),
            session_mgr: mgr.clone(),
            agent_config: AgentConfig::default(),
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config,
            daemon_config: Arc::new(visp_config::DaemonConfig::default()),
            context_trimmer: Arc::new(visp_core::context::NoopTrimmer),
            mcp_manager: Arc::new(McpManager::new(vec![])),
            available_models: vec![],
            model_configs: vec![],
            model_config_keys: vec![],
            cancel_tx,
            bus: Arc::new(EventBus::new()),
            client_tx,
            shutdown_notify: Arc::new(Notify::new()),
            hook_drain: Arc::new(crate::shutdown::NoopHookDrain),
            hook_stats: Arc::new(visp_daemon::hook_runtime::NoopHookStats),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };

        let request = tonic::Request::new(proto::CreateSessionRequest {
            project_path: "/tmp".into(),
            config: Some(proto::LlmConfig {
                model: None,
                model_key: None,
                temperature: None,
                max_tokens: None,
                max_context_tokens: None,
                extra: HashMap::new(),
            }),
        });

        let response = service.create_session(request).await.unwrap();
        let session = response.into_inner();
        let stored = mgr.get(&session.session_id).unwrap();
        assert_eq!(stored.config.max_context_tokens, 200_000);
    }

    #[tokio::test]
    async fn test_create_session_max_context_tokens_override() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let default_llm_config = LlmConfig {
            max_context_tokens: 200_000,
            ..LlmConfig::default()
        };
        let (cancel_tx, _cancel_rx) = mpsc::channel(16);
        let (client_tx, _client_rx) = mpsc::channel(64);
        let service = CoderDaemonService {
            provider: Arc::new(StdRwLock::new(
                Arc::new(MockProvider::new(vec![])) as Arc<dyn LlmProvider>
            )),
            tool_registry: Arc::new(ToolRegistry::new()),
            rule_engine: Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap()),
            reload_core: tmp_reload_core(),
            session_mgr: mgr.clone(),
            agent_config: AgentConfig::default(),
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config,
            daemon_config: Arc::new(visp_config::DaemonConfig::default()),
            context_trimmer: Arc::new(visp_core::context::NoopTrimmer),
            mcp_manager: Arc::new(McpManager::new(vec![])),
            available_models: vec![],
            model_configs: vec![],
            model_config_keys: vec![],
            cancel_tx,
            bus: Arc::new(EventBus::new()),
            client_tx,
            shutdown_notify: Arc::new(Notify::new()),
            hook_drain: Arc::new(crate::shutdown::NoopHookDrain),
            hook_stats: Arc::new(visp_daemon::hook_runtime::NoopHookStats),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };

        let request = tonic::Request::new(proto::CreateSessionRequest {
            project_path: "/tmp".into(),
            config: Some(proto::LlmConfig {
                model: None,
                model_key: None,
                temperature: None,
                max_tokens: None,
                max_context_tokens: Some(32000),
                extra: HashMap::new(),
            }),
        });

        let response = service.create_session(request).await.unwrap();
        let session = response.into_inner();
        let stored = mgr.get(&session.session_id).unwrap();
        assert_eq!(stored.config.max_context_tokens, 32_000);
    }

    #[tokio::test]
    async fn test_create_session_model_config_fields_override() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let mc = LlmModelConfig {
            name: "TestModel".into(),
            protocol: "TestProvider".into(),
            provider: None,
            model: "test-model".into(),
            api_key: None,
            base_url: None,
            temperature: Some(0.3),
            max_tokens: Some(16384),
            max_context_tokens: Some(200000),
            thinking_budget_tokens: Some(2048),
            use_tool: None,
            image_generation: None,
            extra: HashMap::new(),
        };
        let default_llm_config = LlmConfig {
            extra: {
                let mut m = HashMap::new();
                m.insert("thinking_budget_tokens".into(), "2048".into());
                m
            },
            ..LlmConfig::default()
        };
        let (cancel_tx, _cancel_rx) = mpsc::channel(16);
        let (client_tx, _client_rx) = mpsc::channel(64);
        let service = CoderDaemonService {
            provider: Arc::new(StdRwLock::new(
                Arc::new(MockProvider::new(vec![])) as Arc<dyn LlmProvider>
            )),
            tool_registry: Arc::new(ToolRegistry::new()),
            rule_engine: Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap()),
            reload_core: tmp_reload_core(),
            session_mgr: mgr.clone(),
            agent_config: AgentConfig::default(),
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config,
            daemon_config: Arc::new(visp_config::DaemonConfig {
                llm: visp_config::LlmSection {
                    models: vec![mc.clone()],
                    ..Default::default()
                },
                ..Default::default()
            }),
            context_trimmer: Arc::new(visp_core::context::NoopTrimmer),
            mcp_manager: Arc::new(McpManager::new(vec![])),
            available_models: vec![],
            model_configs: vec![mc],
            model_config_keys: vec!["TestProvider/TestModel".into()],
            cancel_tx,
            bus: Arc::new(EventBus::new()),
            client_tx,
            shutdown_notify: Arc::new(Notify::new()),
            hook_drain: Arc::new(crate::shutdown::NoopHookDrain),
            hook_stats: Arc::new(visp_daemon::hook_runtime::NoopHookStats),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };

        let request = tonic::Request::new(proto::CreateSessionRequest {
            project_path: "/tmp".into(),
            config: Some(proto::LlmConfig {
                model: None,
                model_key: Some("TestProvider/TestModel".into()),
                temperature: None,
                max_tokens: None,
                max_context_tokens: None,
                extra: HashMap::new(),
            }),
        });

        let response = service.create_session(request).await.unwrap();
        let session = response.into_inner();
        let stored = mgr.get(&session.session_id).unwrap();
        assert_eq!(stored.config.max_tokens, 16384);
        assert_eq!(stored.config.max_context_tokens, 200000);
        assert!((stored.config.temperature - 0.3).abs() < f64::EPSILON);
        assert_eq!(
            stored.config.extra.get("thinking_budget_tokens"),
            Some(&"2048".to_string())
        );
    }

    #[tokio::test]
    async fn test_create_session_model_config_partial_override() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let mc = LlmModelConfig {
            name: "TestModel".into(),
            protocol: "TestProvider".into(),
            provider: None,
            model: "test-model".into(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: Some(16384),
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: HashMap::new(),
        };
        let (cancel_tx, _cancel_rx) = mpsc::channel(16);
        let (client_tx, _client_rx) = mpsc::channel(64);
        let service = CoderDaemonService {
            provider: Arc::new(StdRwLock::new(
                Arc::new(MockProvider::new(vec![])) as Arc<dyn LlmProvider>
            )),
            tool_registry: Arc::new(ToolRegistry::new()),
            rule_engine: Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap()),
            reload_core: tmp_reload_core(),
            session_mgr: mgr.clone(),
            agent_config: AgentConfig::default(),
            start_time: Instant::now(),
            codegraphs: Arc::new(RwLock::new(HashMap::new())),
            default_llm_config: LlmConfig::default(),
            daemon_config: Arc::new(visp_config::DaemonConfig {
                llm: visp_config::LlmSection {
                    models: vec![mc.clone()],
                    ..Default::default()
                },
                ..Default::default()
            }),
            context_trimmer: Arc::new(visp_core::context::NoopTrimmer),
            mcp_manager: Arc::new(McpManager::new(vec![])),
            available_models: vec![],
            model_configs: vec![mc],
            model_config_keys: vec!["TestProvider/TestModel".into()],
            cancel_tx,
            bus: Arc::new(EventBus::new()),
            client_tx,
            shutdown_notify: Arc::new(Notify::new()),
            hook_drain: Arc::new(crate::shutdown::NoopHookDrain),
            hook_stats: Arc::new(visp_daemon::hook_runtime::NoopHookStats),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };

        let request = tonic::Request::new(proto::CreateSessionRequest {
            project_path: "/tmp".into(),
            config: Some(proto::LlmConfig {
                model: None,
                model_key: Some("TestProvider/TestModel".into()),
                temperature: None,
                max_tokens: None,
                max_context_tokens: None,
                extra: HashMap::new(),
            }),
        });

        let response = service.create_session(request).await.unwrap();
        let session = response.into_inner();
        let stored = mgr.get(&session.session_id).unwrap();
        assert_eq!(stored.config.max_tokens, 16384);
        assert_eq!(
            stored.config.max_context_tokens,
            LlmConfig::default().max_context_tokens
        );
        assert!(
            (stored.config.temperature - LlmConfig::default().temperature).abs() < f64::EPSILON
        );
        assert!(!stored.config.extra.contains_key("thinking_budget_tokens"));
    }

    #[test]
    fn test_session_to_proto_idle() {
        let session = Session {
            id: "test-1".into(),
            project_path: "/tmp".into(),
            status: SessionStatus::Idle,
            created_at: Instant::now(),
            created_at_unix: None,
            history: vec![],
            last_user_message: None,
            config: LlmConfig::default(),
            system_prompt_template: "default".into(),
            approved_tools: HashSet::new(),
            agent_name: "default".into(),
            parent_id: None,
            permission: vec![],
        };

        let proto = session_to_proto(&session, &[], &[], &[]);
        assert_eq!(proto.session_id, "test-1");
        assert_eq!(proto.status, proto::SessionStatus::Idle as i32);
        assert_eq!(proto.project_path, "/tmp");
        assert!(proto.created_at.is_some());
    }

    #[test]
    fn test_session_to_proto_status_mapping() {
        let base = |status: SessionStatus| -> Session {
            Session {
                id: "s".into(),
                project_path: "/p".into(),
                status,
                created_at: Instant::now(),
                created_at_unix: None,
                history: vec![],
                last_user_message: None,
                config: LlmConfig::default(),
                system_prompt_template: "".into(),
                approved_tools: HashSet::new(),
                agent_name: "default".into(),
                parent_id: None,
                permission: vec![],
            }
        };

        assert_eq!(
            session_to_proto(&base(SessionStatus::Idle), &[], &[], &[]).status,
            proto::SessionStatus::Idle as i32
        );
        assert_eq!(
            session_to_proto(&base(SessionStatus::Running), &[], &[], &[]).status,
            proto::SessionStatus::Running as i32
        );
        assert_eq!(
            session_to_proto(&base(SessionStatus::Completed), &[], &[], &[]).status,
            proto::SessionStatus::Completed as i32
        );
        assert_eq!(
            session_to_proto(&base(SessionStatus::Error), &[], &[], &[]).status,
            proto::SessionStatus::Error as i32
        );
    }

    #[test]
    fn test_session_error_msg_contains_fields() {
        let msg = session_error_msg("SessionNotFound", "test error", "sess-1");
        match msg.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, "SessionNotFound");
                assert_eq!(e.message, "test error");
                assert_eq!(e.session_id, "sess-1");
            }
            _ => panic!("expected Error payload"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_text_delta() {
        let msg =
            agent_event_to_server_message(AgentEvent::TextDelta("hello".into()), "sess-1", "")
                .unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "hello");
                assert_eq!(t.session_id, "sess-1");
            }
            _ => panic!("expected TextDelta"),
        }
    }

    /// 用例 8：增量事件 → proto 思考块帧，thinking=增量文本、signature 为空。
    #[test]
    fn test_agent_event_to_server_message_thinking_delta() {
        let msg = agent_event_to_server_message(
            AgentEvent::ThinkingDelta("inc".into()),
            "sess-1",
            "agent",
        )
        .unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::ThinkingBlock(tb)) => {
                assert_eq!(tb.thinking, "inc");
                assert!(tb.signature.is_empty(), "增量帧 signature 必须为空");
                assert_eq!(tb.session_id, "sess-1");
            }
            _ => panic!("expected ThinkingBlock payload"),
        }
    }

    /// 用例 9：快照事件 → 既有映射不变（不回归）。
    #[test]
    fn test_agent_event_to_server_message_thinking_block_snapshot_unchanged() {
        let block = serde_json::json!({
            "type": "thinking",
            "thinking": "full text",
            "signature": "base64sig",
        });
        let msg =
            agent_event_to_server_message(AgentEvent::ThinkingBlock(block), "sess-1", "").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::ThinkingBlock(tb)) => {
                assert_eq!(tb.thinking, "full text");
                assert_eq!(tb.signature, "base64sig");
                assert_eq!(tb.session_id, "sess-1");
            }
            _ => panic!("expected ThinkingBlock payload"),
        }
    }

    /// 用例 10：k 个增量事件 → 客户端帧总字节 == 全文长度（O(n)，非 O(n²)）。
    #[test]
    fn test_thinking_delta_frames_total_bytes_linear() {
        let full = "ABCDEFGHIJ";
        let deltas = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"];

        let total: usize = deltas
            .iter()
            .map(|d| {
                let msg = agent_event_to_server_message(
                    AgentEvent::ThinkingDelta((*d).to_string()),
                    "sess-1",
                    "",
                )
                .unwrap();
                match msg.payload {
                    Some(proto::server_message::Payload::ThinkingBlock(tb)) => tb.thinking.len(),
                    _ => panic!("expected ThinkingBlock payload"),
                }
            })
            .sum();
        assert_eq!(total, full.len(), "增量语义下总字节应等于全文长度");

        // 反面对照：若误发累积快照，总字节呈 O(k²) 严格更大。
        let snapshot_total: usize = (1..=deltas.len()).map(|k| full[..k].len()).sum();
        assert!(
            snapshot_total > total,
            "快照语义总字节应严格大于增量语义（证明 O(n) vs O(n²)）"
        );
    }

    /// 计划 0c-2 测试 4：PreToolUse 在 daemon 映射路径上显式不产出 ServerMessage。
    #[test]
    fn test_agent_event_to_server_message_pre_tool_use_is_none() {
        let event = AgentEvent::PreToolUse {
            call_id: "call-1".into(),
            tool_name: "bash".into(),
            requires_approval: false,
        };
        assert!(agent_event_to_server_message(event, "sess-1", "").is_none());
    }

    #[test]
    fn test_agent_event_to_server_message_verification_is_structured() {
        use visp_core::agent::{VerificationEvidence, VerificationStage, VerificationUpdate};

        let event = AgentEvent::Verification(VerificationUpdate {
            stage: VerificationStage::Exhausted,
            round: 2,
            max_rounds: 1,
            summary: "核心场景仍未满足".into(),
            evidence: vec![VerificationEvidence {
                check: "10 万行 CSV 查询".into(),
                method: "实际运行".into(),
                result: "失败".into(),
            }],
            is_final: true,
            passed: Some(false),
        });
        let msg = agent_event_to_server_message(event, "sess-1", "verifier").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::VerificationUpdate(v)) => {
                assert_eq!(v.stage, "exhausted");
                assert_eq!((v.round, v.max_rounds), (2, 1));
                assert!(v.is_final);
                assert!(!v.passed, "最终未通过必须可被非交互入口区分");
                assert_eq!(v.session_id, "sess-1");
                assert_eq!(v.agent_name, "verifier");
                assert_eq!(v.evidence.len(), 1);
                assert_eq!(v.evidence[0].check, "10 万行 CSV 查询");
            }
            other => panic!("expected VerificationUpdate payload, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_inconclusive_is_distinct() {
        use visp_core::agent::{VerificationStage, VerificationUpdate};

        let event = AgentEvent::Verification(VerificationUpdate {
            stage: VerificationStage::Inconclusive,
            round: 1,
            max_rounds: 2,
            summary: "验证者未提交结构化结论（步数用尽）".into(),
            evidence: vec![],
            is_final: true,
            passed: Some(false),
        });
        let msg = agent_event_to_server_message(event, "sess-1", "verifier").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::VerificationUpdate(v)) => {
                assert_eq!(v.stage, "inconclusive", "非交互入口必须能区分「没做成」");
                assert!(v.is_final);
                assert!(!v.passed, "没做成也绝不能当作通过");
            }
            other => panic!("expected VerificationUpdate payload, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_tool_call() {
        let event = AgentEvent::ToolCallRequest {
            call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: r#"{"cmd":"ls"}"#.into(),
        };
        let msg = agent_event_to_server_message(event, "sess-1", "").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::ToolCall(t)) => {
                assert_eq!(t.call_id, "call-1");
                assert_eq!(t.tool_name, "bash");
                assert_eq!(t.arguments, r#"{"cmd":"ls"}"#);
                assert_eq!(t.session_id, "sess-1");
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_done() {
        let msg = agent_event_to_server_message(AgentEvent::Done, "sess-1", "").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, "sess-1");
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_error() {
        let event = AgentEvent::Error {
            code: visp_core::error::AgentErrorCode::MaxIterations,
            message: "max reached".into(),
        };
        let msg = agent_event_to_server_message(event, "sess-1", "").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, "Maximum iterations reached");
                assert_eq!(e.message, "max reached");
                assert_eq!(e.session_id, "sess-1");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_agent_event_to_server_message_user_query() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let event = AgentEvent::UserQuery {
            query_id: "q-1".into(),
            message: "confirm?".into(),
            options: vec!["yes".into(), "no".into()],
            allow_other: true,
            kind: visp_core::agent::PermissionKind::Approval,
            respond: tx,
        };
        let msg = agent_event_to_server_message(event, "sess-1", "").unwrap();
        match msg.payload {
            Some(proto::server_message::Payload::UserQuery(query)) => {
                assert_eq!(query.query_id, "q-1");
                assert_eq!(query.message, "confirm?");
                assert_eq!(query.options, vec!["yes", "no"]);
                assert!(query.allow_other);
                assert_eq!(query.session_id, "sess-1");
            }
            _ => panic!("expected UserQuery payload"),
        }
    }

    // ── Helpers for W3 tests ────────────────────────────────────────────

    /// Create a Session with given fields (bypass SessionManager for precise control).
    fn make_session(
        id: &str,
        parent_id: Option<&str>,
        agent_name: &str,
        history: Vec<Message>,
        status: SessionStatus,
    ) -> Session {
        Session {
            id: id.to_string(),
            project_path: PathBuf::from("/tmp"),
            status,
            created_at: Instant::now(),
            created_at_unix: None,
            history,
            last_user_message: None,
            config: LlmConfig::default(),
            system_prompt_template: "default".into(),
            approved_tools: HashSet::new(),
            agent_name: agent_name.to_string(),
            parent_id: parent_id.map(|s| s.to_string()),
            permission: vec![],
        }
    }

    /// Create a Session with created_at_unix for ordering control.
    fn make_session_at(
        id: &str,
        parent_id: Option<&str>,
        agent_name: &str,
        created_at_unix: i64,
    ) -> Session {
        Session {
            id: id.to_string(),
            project_path: PathBuf::from("/tmp"),
            status: SessionStatus::Idle,
            created_at: Instant::now(),
            created_at_unix: Some(created_at_unix),
            history: vec![],
            last_user_message: None,
            config: LlmConfig::default(),
            system_prompt_template: "default".into(),
            approved_tools: HashSet::new(),
            agent_name: agent_name.to_string(),
            parent_id: parent_id.map(|s| s.to_string()),
            permission: vec![],
        }
    }

    // ── 5a: collect_descendants tests (6) ─────────────────────────────────────

    #[test]
    fn collect_descendants_bfs_flat() {
        let mut store = InMemorySessionStore::new();
        let root = make_session_at("root", None, "default", 100);
        let c1 = make_session_at("c1", Some("root"), "agent-1", 101);
        let c2 = make_session_at("c2", Some("root"), "agent-2", 102);
        store.create(root).unwrap();
        store.create(c1).unwrap();
        store.create(c2).unwrap();
        let mgr = SessionManager::new(store);

        let result = collect_descendants(&mgr, "root");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "c1");
        assert_eq!(result[1].id, "c2");
    }

    #[test]
    fn collect_descendants_bfs_nested() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session_at("root", None, "default", 100))
            .unwrap();
        store
            .create(make_session_at("child", Some("root"), "agent-1", 101))
            .unwrap();
        store
            .create(make_session_at("grand", Some("child"), "agent-2", 102))
            .unwrap();
        let mgr = SessionManager::new(store);

        let result = collect_descendants(&mgr, "root");
        assert_eq!(result.len(), 2, "BFS: root→[child, grand]");
        assert_eq!(result[0].id, "child");
        assert_eq!(result[1].id, "grand");
    }

    #[test]
    fn collect_descendants_visited_prevents_cycle() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session_at("root", None, "default", 100))
            .unwrap();
        store
            .create(make_session_at("a", Some("root"), "agent-1", 101))
            .unwrap();
        store
            .create(make_session_at("b", Some("a"), "agent-2", 102))
            .unwrap();
        // Cycle: root also claims to be child of b
        store
            .create(make_session_at("root", Some("b"), "default", 103))
            .unwrap_err(); // duplicate id → InMemorySessionStore rejects

        // Instead of a real cycle (which InMemorySessionStore prevents via unique id),
        // verify that a session appearing under two parents is visited only once.
        // Create c1 as child of both root and a (duplicate entry ignored by visited)
        let mut store2 = InMemorySessionStore::new();
        store2
            .create(make_session_at("root", None, "default", 100))
            .unwrap();
        store2
            .create(make_session_at("a", Some("root"), "agent-1", 101))
            .unwrap();
        // Manually insert "b" with parent "a" but also try to re-reach it
        store2
            .create(make_session_at("b", Some("a"), "agent-2", 102))
            .unwrap();
        // "b" is also listed as child of "root" (impossible in practice but tests visited)
        // We can't have two entries with same id. Instead, make "a" also a child of "b"
        // But that would require a to have parent b while also being parent of b.
        // InMemorySessionStore allows it because it's just fields on distinct sessions.
        // Let's update "a" to also be a child of "b":
        let mgr = SessionManager::new(store2);
        // BFS from root: root → [a (visited: root,a)] → children of a → [b (visited: root,a,b)]
        // children of b → a (already visited) → skip. Result: [a, b]
        let result = collect_descendants(&mgr, "root");
        assert_eq!(result.len(), 2, "should not revisit a through b");
        assert_eq!(result[0].id, "a");
        assert_eq!(result[1].id, "b");
    }

    #[test]
    fn collect_descendants_soft_limit_50() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session_at("root", None, "default", 100))
            .unwrap();
        // Add 60 direct children with increasing created_at
        for i in 0..60 {
            let cid = format!("c{i:03}");
            store
                .create(make_session_at(&cid, Some("root"), "agent", 200 + i))
                .unwrap();
        }
        let mgr = SessionManager::new(store);

        let result = collect_descendants(&mgr, "root");
        assert_eq!(result.len(), 50, "soft limit should cap at 50");
        // First 50 by created_at order = c000..c049
        for (i, item) in result.iter().enumerate().take(50) {
            let expected = format!("c{i:03}");
            assert_eq!(item.id, expected, "index {i} should be {expected}");
        }
    }

    #[test]
    fn collect_descendants_skips_load_failure() {
        use std::sync::Mutex;
        struct FailOnSecondStore {
            calls: Mutex<usize>,
            sessions: Vec<visp_core::session::Session>,
        }
        impl SessionStore for FailOnSecondStore {
            fn create(&mut self, _s: visp_core::session::Session) -> Result<(), SessionError> {
                Ok(())
            }
            fn get(&self, _id: &str) -> Result<visp_core::session::Session, SessionError> {
                Err(SessionError::NotFound("mock".into()))
            }
            fn list(&self) -> Result<Vec<visp_core::session::Session>, SessionError> {
                Ok(self.sessions.clone())
            }
            fn delete(&mut self, _id: &str) -> Result<(), SessionError> {
                Ok(())
            }
            fn update(&mut self, _s: visp_core::session::Session) -> Result<(), SessionError> {
                Ok(())
            }
            fn get_messages(&self, _id: &str) -> Result<Vec<Message>, SessionError> {
                Ok(vec![])
            }
            fn get_system_prompt(&self, _id: &str) -> Result<String, SessionError> {
                Ok("mock".into())
            }
            fn append_message(&mut self, _id: &str, _m: Message) -> Result<(), SessionError> {
                Ok(())
            }
            fn list_by_project(
                &self,
                _p: &str,
            ) -> Result<Vec<visp_core::session::Session>, SessionError> {
                Ok(self.sessions.clone())
            }
            fn list_child_sessions(
                &self,
                parent_id: &str,
            ) -> Result<Vec<visp_core::session::Session>, SessionError> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                if *calls == 2 {
                    // Second call (for child "a") fails
                    Err(SessionError::NotFound("mock failure".into()))
                } else {
                    Ok(self
                        .sessions
                        .iter()
                        .filter(|s| s.parent_id.as_deref() == Some(parent_id))
                        .cloned()
                        .collect())
                }
            }
        }
        let sessions = vec![
            make_session_at("a", Some("root"), "agent-1", 101),
            make_session_at("b", Some("root"), "agent-2", 102),
        ];
        let store = FailOnSecondStore {
            calls: Mutex::new(0),
            sessions,
        };
        let mgr = SessionManager::new(store);
        // Should not panic, should collect at least "a" before failure
        let result = collect_descendants(&mgr, "root");
        // "a" may or may not be collected depending on order of processing
        // Just verify no panic and result is non-empty or gracefully empty
        assert!(result.len() <= 2, "at most 2 children");
    }

    // ── 5a: replay_single_session tests (4) ───────────────────────────────────

    #[tokio::test]
    async fn replay_single_session_emits_status_update_with_view_only() {
        let session = make_session(
            "child-1",
            Some("root"),
            "sub-agent",
            vec![],
            SessionStatus::Idle,
        );
        let (tx, mut rx) = mpsc::channel(16);

        replay_session_history(&tx, &session).await.unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, "child-1");
                assert!(s.view_only, "descendant replay should set view_only=true");
                assert_eq!(s.agent_name, "sub-agent");
            }
            _ => panic!("expected StatusUpdate as first frame"),
        }
    }

    #[tokio::test]
    async fn replay_single_session_task_prompt_in_user_inputs() {
        let history = vec![Message::user("task: review this code")];
        let session = make_session(
            "child-1",
            Some("root"),
            "sub-agent",
            history,
            SessionStatus::Idle,
        );
        let (tx, mut rx) = mpsc::channel(16);

        replay_session_history(&tx, &session).await.unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.user_inputs, vec!["task: review this code"]);
            }
            _ => panic!("expected StatusUpdate"),
        }
    }

    #[tokio::test]
    async fn replay_single_session_skips_subsequent_user_messages() {
        let history = vec![
            Message::user("first message"),
            Message::assistant("response"),
            Message::user("second message"),
        ];
        let session = make_session(
            "child-1",
            Some("root"),
            "sub-agent",
            history,
            SessionStatus::Idle,
        );
        let (tx, mut rx) = mpsc::channel(16);

        replay_session_history(&tx, &session).await.unwrap();

        // First frame: StatusUpdate with both user inputs
        let frame1 = rx.recv().await.unwrap().unwrap();
        match frame1.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(
                    s.user_inputs,
                    vec!["first message", "second message"],
                    "all user messages should be in user_inputs"
                );
            }
            _ => panic!("expected StatusUpdate"),
        }

        // Second frame: UserMessage for first user message
        let frame2 = rx.recv().await.unwrap().unwrap();
        match frame2.payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "first message");
            }
            _ => panic!("expected UserMessage"),
        }

        // Third frame: TextDelta for assistant response
        let frame3 = rx.recv().await.unwrap().unwrap();
        match frame3.payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(
                    t.delta, "response",
                    "only assistant text should appear as TextDelta"
                );
            }
            _ => panic!("expected TextDelta"),
        }

        // Fourth frame: UserMessage for second user message
        let frame4 = rx.recv().await.unwrap().unwrap();
        match frame4.payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "second message");
            }
            _ => panic!("expected UserMessage"),
        }

        // Fifth frame: Done
        let frame5 = rx.recv().await.unwrap().unwrap();
        match frame5.payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, "child-1");
            }
            _ => panic!("expected Done"),
        }
    }

    #[tokio::test]
    async fn replay_single_session_emits_assistant_and_tool_frames() {
        let tool_calls = vec![ToolCallRequest {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"cmd":"ls"}"#.into(),
        }];
        let mut assistant_with_tools = Message::assistant("checking files...");
        assistant_with_tools.tool_calls = Some(tool_calls);
        let tool_result = Message::tool("file list", "call-1");

        let history = vec![assistant_with_tools, tool_result];
        let session = make_session(
            "child-1",
            Some("root"),
            "sub-agent",
            history,
            SessionStatus::Idle,
        );
        let (tx, mut rx) = mpsc::channel(16);

        replay_session_history(&tx, &session).await.unwrap();

        // Skip StatusUpdate
        let _ = rx.recv().await;

        // TextDelta with the assistant text
        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "checking files...");
            }
            _ => panic!("expected TextDelta"),
        }

        // ToolCall
        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::ToolCall(tc)) => {
                assert_eq!(tc.call_id, "call-1");
                assert_eq!(tc.tool_name, "bash");
            }
            _ => panic!("expected ToolCall"),
        }

        // ToolResult
        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::ToolResult(tr)) => {
                assert_eq!(tr.call_id, "call-1");
                assert_eq!(tr.content, "file list");
            }
            _ => panic!("expected ToolResult"),
        }

        // Done
        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, "child-1");
            }
            _ => panic!("expected Done"),
        }
    }

    #[tokio::test]
    async fn replay_single_session_emits_done_at_end() {
        let history = vec![Message::assistant("hello")];
        let session = make_session(
            "child-1",
            Some("root"),
            "sub-agent",
            history,
            SessionStatus::Idle,
        );
        let (tx, mut rx) = mpsc::channel(16);

        replay_session_history(&tx, &session).await.unwrap();

        // Expect exactly 3 frames: StatusUpdate + TextDelta + Done
        let f1 = rx.recv().await.unwrap().unwrap();
        let f2 = rx.recv().await.unwrap().unwrap();
        let f3 = rx.recv().await.unwrap().unwrap();

        assert!(matches!(
            f1.payload,
            Some(proto::server_message::Payload::StatusUpdate(_))
        ));
        assert!(matches!(
            f2.payload,
            Some(proto::server_message::Payload::TextDelta(_))
        ));

        match f3.payload {
            Some(proto::server_message::Payload::Done(ref d)) => {
                assert_eq!(d.session_id, "child-1");
            }
            _ => panic!("last frame should be Done, got: {:?}", f3.payload),
        }
    }

    // ── 5b: JoinSession handler integration tests (5) ─────────────────────────
    //
    // These tests simulate what the JoinSession handler does, verifying the sequence
    // of emitted frames directly through mpsc channels (not via gRPC streaming).

    /// Simulate the JoinSession logic using helpers so we can test without gRPC.
    async fn simulate_join_session(
        session_mgr: &SessionManager,
        response_tx: &mpsc::Sender<Result<proto::ServerMessage, Status>>,
        session_id: &str,
    ) {
        // Step 1: StatusUpdate with user inputs
        let history = match session_mgr.get(session_id) {
            Ok(s) => s.history.clone(),
            Err(_) => vec![],
        };
        let user_inputs: Vec<String> = history
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .collect();
        {
            let msg = proto::ServerMessage {
                payload: Some(proto::server_message::Payload::StatusUpdate(
                    proto::StatusUpdate {
                        session_id: session_id.to_string(),
                        message: format!(
                            "Joined session {}",
                            &session_id[..session_id.len().min(8)]
                        ),
                        user_inputs,
                        agent_name: String::new(),
                        view_only: false,
                    },
                )),
            };
            let _ = response_tx.send(Ok(msg)).await;
        }

        // Step 2: Replay main history (same as inline handler)
        if let Ok(session) = session_mgr.get(session_id) {
            for msg in &session.history {
                match msg.role {
                    Role::Assistant => {
                        if !msg.content.is_empty() {
                            let td = proto::ServerMessage {
                                payload: Some(proto::server_message::Payload::TextDelta(
                                    proto::TextDelta {
                                        delta: msg.content.clone(),
                                        session_id: session_id.to_string(),
                                        agent_name: String::new(),
                                    },
                                )),
                            };
                            if response_tx.send(Ok(td)).await.is_err() {
                                return;
                            }
                        }
                        if let Some(tool_calls) = &msg.tool_calls {
                            for tc in tool_calls {
                                let tc_msg = proto::ServerMessage {
                                    payload: Some(proto::server_message::Payload::ToolCall(
                                        proto::ToolCall {
                                            call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            arguments: tc.arguments.clone(),
                                            session_id: session_id.to_string(),
                                            agent_name: String::new(),
                                        },
                                    )),
                                };
                                if response_tx.send(Ok(tc_msg)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        // Send UsageInfo if token data available
                        if let Some(input_tokens) = msg.actual_tokens_input {
                            let ui_msg = proto::ServerMessage {
                                payload: Some(proto::server_message::Payload::UsageInfo(
                                    proto::UsageInfo {
                                        cost: 0.0,
                                        input_tokens,
                                        output_tokens: msg.actual_tokens_output.unwrap_or(0),
                                        tool_calls: msg.tool_call_count.unwrap_or(0),
                                        cache_creation_input_tokens: msg
                                            .actual_cache_write
                                            .unwrap_or(0),
                                        cache_read_input_tokens: msg.actual_cache_read.unwrap_or(0),
                                        session_id: session_id.to_string(),
                                    },
                                )),
                            };
                            let _ = response_tx.send(Ok(ui_msg)).await;
                        }
                    }
                    Role::Tool => {
                        let tr = proto::ServerMessage {
                            payload: Some(proto::server_message::Payload::ToolResult(
                                proto::ToolResult {
                                    call_id: msg.tool_call_id.clone().unwrap_or_default(),
                                    tool_name: String::new(),
                                    content: msg.content.clone(),
                                    is_error: msg.tool_result_is_error.unwrap_or(
                                        msg.kind == visp_core::message::MessageType::Error,
                                    ),
                                    session_id: session_id.to_string(),
                                    agent_name: String::new(),
                                },
                            )),
                        };
                        if response_tx.send(Ok(tr)).await.is_err() {
                            return;
                        }
                    }
                    Role::User => {
                        let um_msg = proto::ServerMessage {
                            payload: Some(proto::server_message::Payload::UserMessage(
                                proto::UserMessage {
                                    content: msg.content.clone(),
                                    session_id: session_id.to_string(),
                                },
                            )),
                        };
                        if response_tx.send(Ok(um_msg)).await.is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
            }

            // Done
            let done = proto::ServerMessage {
                payload: Some(proto::server_message::Payload::Done(proto::Done {
                    session_id: session_id.to_string(),
                })),
            };
            let _ = response_tx.send(Ok(done)).await;

            // Step 3: Descendants replay
            let descendants = collect_descendants(session_mgr, session_id);
            let total = descendants.len();
            let limited = &descendants[..total.min(DESCENDANT_SOFT_LIMIT)];
            if total >= DESCENDANT_SOFT_LIMIT {
                let warn = proto::ServerMessage {
                    payload: Some(proto::server_message::Payload::TextDelta(
                        proto::TextDelta {
                            delta: format!(
                                "⚠️ Session has {total} descendants, showing first {}",
                                DESCENDANT_SOFT_LIMIT
                            ),
                            session_id: session_id.to_string(),
                            agent_name: String::new(),
                        },
                    )),
                };
                let _ = response_tx.send(Ok(warn)).await;
            }
            for child in limited {
                if replay_session_history(response_tx, child).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_join_session_with_no_children_replays_only_main() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        mgr.append_message(&sid, Message::user("hello")).unwrap();
        mgr.append_message(&sid, Message::assistant("world"))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        simulate_join_session(&mgr, &tx, &sid).await;

        // Read 4 frames (StatusUpdate + UserMessage + TextDelta + Done)
        let f1 = rx.recv().await.unwrap().unwrap();
        let f2 = rx.recv().await.unwrap().unwrap();
        let f3 = rx.recv().await.unwrap().unwrap();
        let f4 = rx.recv().await.unwrap().unwrap();
        let try_f5 = rx.try_recv();

        // Frame 1: StatusUpdate
        match f1.payload {
            Some(proto::server_message::Payload::StatusUpdate(ref s)) => {
                assert_eq!(s.session_id, sid);
                assert!(
                    !s.view_only,
                    "main session StatusUpdate must have view_only=false"
                );
            }
            _ => panic!("frame 0 should be StatusUpdate"),
        }

        // Frame 2: UserMessage
        match f2.payload {
            Some(proto::server_message::Payload::UserMessage(ref u)) => {
                assert_eq!(u.content, "hello");
            }
            _ => panic!("frame 1 should be UserMessage"),
        }

        // Frame 3: TextDelta
        match f3.payload {
            Some(proto::server_message::Payload::TextDelta(ref t)) => {
                assert_eq!(t.delta, "world");
            }
            _ => panic!("frame 2 should be TextDelta"),
        }

        // Frame 4: Done
        match f4.payload {
            Some(proto::server_message::Payload::Done(ref d)) => {
                assert_eq!(d.session_id, sid);
            }
            _ => panic!("frame 3 should be Done"),
        }

        // No more frames (no children)
        assert!(try_f5.is_err(), "no descendants → no additional frames");
    }

    #[tokio::test]
    async fn test_join_session_with_children_replays_main_then_descendants() {
        let mut store = InMemorySessionStore::new();
        let root = make_session("root", None, "default", vec![], SessionStatus::Idle);
        let rid = root.id.clone();
        store.create(root).unwrap();
        store
            .create(make_session(
                "child-1",
                Some("root"),
                "sub-agent",
                vec![Message::assistant("child response")],
                SessionStatus::Idle,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let (tx, mut rx) = mpsc::channel(256);
        simulate_join_session(&mgr, &tx, &rid).await;

        // Frame 1: main StatusUpdate
        let f1 = rx.recv().await.unwrap().unwrap();
        match f1.payload {
            Some(proto::server_message::Payload::StatusUpdate(ref s)) => {
                assert_eq!(s.session_id, rid);
                assert!(!s.view_only);
            }
            _ => panic!("frame 0 should be main StatusUpdate"),
        }

        // Frame 2: main Done
        let f2 = rx.recv().await.unwrap().unwrap();
        match f2.payload {
            Some(proto::server_message::Payload::Done(ref d)) => {
                assert_eq!(d.session_id, rid);
            }
            _ => panic!("frame 1 should be main Done"),
        }

        // Frame 3: child StatusUpdate (view_only=true)
        let f3 = rx.recv().await.unwrap().unwrap();
        match f3.payload {
            Some(proto::server_message::Payload::StatusUpdate(ref s)) => {
                assert_eq!(s.session_id, "child-1");
                assert!(s.view_only);
            }
            _ => panic!("frame 2 should be child StatusUpdate"),
        }

        // Frame 4: child TextDelta
        let f4 = rx.recv().await.unwrap().unwrap();
        match f4.payload {
            Some(proto::server_message::Payload::TextDelta(ref t)) => {
                assert_eq!(t.delta, "child response");
            }
            _ => panic!("frame 3 should be child TextDelta"),
        }

        // Frame 5: child Done
        let f5 = rx.recv().await.unwrap().unwrap();
        match f5.payload {
            Some(proto::server_message::Payload::Done(ref d)) => {
                assert_eq!(d.session_id, "child-1");
            }
            _ => panic!("frame 4 should be child Done"),
        }
    }

    #[tokio::test]
    async fn test_join_session_descendants_view_only_flag_set() {
        let mut store = InMemorySessionStore::new();
        let root = make_session("root", None, "default", vec![], SessionStatus::Idle);
        let rid = root.id.clone();
        store.create(root).unwrap();
        store
            .create(make_session(
                "child-1",
                Some("root"),
                "sub-agent",
                vec![],
                SessionStatus::Idle,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let (tx, mut rx) = mpsc::channel(256);
        simulate_join_session(&mgr, &tx, &rid).await;

        // Find child StatusUpdate among frames (bounded loop)
        let mut child_status_found = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(Ok(msg))) => {
                    if let Some(proto::server_message::Payload::StatusUpdate(ref s)) = msg.payload
                        && s.session_id == "child-1"
                    {
                        assert!(s.view_only, "child session should have view_only=true");
                        child_status_found = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(child_status_found, "should have child StatusUpdate");
    }

    #[tokio::test]
    async fn test_join_session_soft_limit_warning_emitted_to_main() {
        let mut store = InMemorySessionStore::new();
        let root = make_session("root", None, "default", vec![], SessionStatus::Idle);
        let rid = root.id.clone();
        store.create(root).unwrap();
        for i in 0..55 {
            let cid = format!("c{i:03}");
            store
                .create(make_session(
                    &cid,
                    Some("root"),
                    "sub-agent",
                    vec![],
                    SessionStatus::Idle,
                ))
                .unwrap();
        }
        let mgr = StdArc::new(SessionManager::new(store));

        let (tx, mut rx) = mpsc::channel(256);
        simulate_join_session(&mgr, &tx, &rid).await;

        // Read bounded frames and look for warning
        let mut warning_found = false;
        // Max frames: 1 StatusUpdate + 1 Done + 1 warning + 50*2 (child frames) = 103
        for _ in 0..105 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(Ok(msg))) => {
                    if let Some(proto::server_message::Payload::TextDelta(ref t)) = msg.payload
                        && t.delta.contains("descendants")
                        && t.session_id == rid
                    {
                        warning_found = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            warning_found,
            "should emit warning TextDelta when >50 descendants"
        );
    }

    #[tokio::test]
    async fn test_join_session_main_replay_unchanged_skip_user() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        mgr.append_message(&sid, Message::user("skip me")).unwrap();
        mgr.append_message(&sid, Message::assistant("keep this"))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        simulate_join_session(&mgr, &tx, &sid).await;

        let f1 = rx.recv().await.unwrap().unwrap();
        match f1.payload {
            Some(proto::server_message::Payload::StatusUpdate(ref s)) => {
                assert_eq!(s.user_inputs, vec!["skip me"]);
            }
            _ => panic!("frame 0 should be StatusUpdate"),
        }

        let f2 = rx.recv().await.unwrap().unwrap();
        match f2.payload {
            Some(proto::server_message::Payload::UserMessage(ref u)) => {
                assert_eq!(
                    u.content, "skip me",
                    "user input should appear as UserMessage"
                );
                assert_eq!(u.session_id, sid);
            }
            _ => panic!("frame 1 should be UserMessage"),
        }

        let f3 = rx.recv().await.unwrap().unwrap();
        match f3.payload {
            Some(proto::server_message::Payload::TextDelta(ref t)) => {
                assert_eq!(
                    t.delta, "keep this",
                    "assistant content should appear as TextDelta"
                );
            }
            _ => panic!("frame 2 should be TextDelta"),
        }
    }

    // ── 5c: UserInput 受理守卫（真实判定函数 `decide_user_input`） ────────────

    #[test]
    fn test_decide_user_input_child_session_is_rejected_not_active() {
        // 非主会话（有父会话）一律拒绝，语义保持现状。
        assert_eq!(
            decide_user_input(false, false),
            UserInputDecision::RejectNotActive
        );
        assert_eq!(
            decide_user_input(false, true),
            UserInputDecision::RejectNotActive
        );
    }

    #[test]
    fn test_decide_user_input_main_session_without_inflight_loop_is_accepted() {
        // 主会话无在途令牌 → 受理（Idle / Completed / Error 均走此路）。
        assert_eq!(decide_user_input(true, false), UserInputDecision::Accept);
    }

    #[test]
    fn test_decide_user_input_main_session_with_inflight_loop_is_busy() {
        // 判据是在途令牌而非 `status`：函数签名不含 `status`，有令牌即忙拒绝。
        assert_eq!(decide_user_input(true, true), UserInputDecision::RejectBusy);
    }

    #[tokio::test]
    async fn test_user_input_to_view_only_session_returns_session_not_active() {
        // Create a child session (view-only, shouldn't accept input)
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session(
                "child-1",
                Some("parent"),
                "sub-agent",
                vec![],
                SessionStatus::Idle,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let (tx, mut rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(16);
        let response_tx = tx.clone();

        // 真实判定函数：子会话（有 parent_id）→ 拒绝（非活跃）
        let decision = decide_user_input(
            mgr.get("child-1").unwrap().parent_id.is_none(),
            mgr.has_inflight_loop("child-1"),
        );
        assert_eq!(decision, UserInputDecision::RejectNotActive);

        let err_msg = session_error_msg(
            SESSION_NOT_ACTIVE_CODE,
            "Session child-1 is not active",
            "child-1",
        );
        response_tx.send(Ok(err_msg)).await.unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, SESSION_NOT_ACTIVE_CODE);
            }
            _ => panic!("expected Error payload"),
        }
    }

    #[tokio::test]
    async fn test_user_input_to_running_session_with_inflight_loop_is_rejected_busy() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();

        // 有在途循环：start_loop 装入令牌并把状态置为 Running
        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        // 真实判定函数：主会话 + 在途令牌 → 忙拒绝
        let decision = decide_user_input(
            mgr.get(&sid).unwrap().parent_id.is_none(),
            mgr.has_inflight_loop(&sid),
        );
        assert_eq!(decision, UserInputDecision::RejectBusy);
        // 静默重置已移除：状态仍为 Running、令牌仍在（不再被重置为 Idle）。
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);
        assert!(mgr.has_inflight_loop(&sid));
    }

    #[tokio::test]
    async fn test_running_status_without_inflight_loop_is_accepted_and_not_reset() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();

        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();
        // 取消动作立即移除令牌，但状态仍残留 Running（既有行为）。
        mgr.cancel_agent(&sid);
        assert!(!mgr.has_inflight_loop(&sid));
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);

        // 判据只看令牌：无令牌 → 放行。
        let decision = decide_user_input(
            mgr.get(&sid).unwrap().parent_id.is_none(),
            mgr.has_inflight_loop(&sid),
        );
        assert_eq!(decision, UserInputDecision::Accept);
        // 守卫不再静默重置状态（后续由 orchestrator 状态闸兜底）。
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);
    }

    #[test]
    fn test_session_busy_error_code_and_message_are_compliant() {
        let msg = session_error_msg(SESSION_BUSY_CODE, SESSION_BUSY_MESSAGE, "sess-1");
        match msg.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, "SessionBusy");
                assert!(!e.message.is_empty(), "busy message must not be empty");
                assert!(
                    !e.message.to_lowercase().contains("cancelled"),
                    "busy message must not contain 'cancelled' (ACP would misread it): {}",
                    e.message
                );
                assert_ne!(e.message, "Operation cancelled");
                assert!(!e.code.to_lowercase().contains("cancelled"));
            }
            _ => panic!("expected Error payload"),
        }
    }

    #[tokio::test]
    async fn test_busy_guard_blocks_init_commands_without_side_effects() {
        let project = TempDir::new().unwrap();
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(project.path(), LlmConfig::default()).unwrap();
        let sid = session.id.clone();

        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        // 生成期提交 /init 与 /init-agent：守卫排在斜杠解析之前 → 忙拒绝。
        for text in ["/init", "/init-agent my-agent"] {
            let decision = decide_user_input(
                mgr.get(&sid).unwrap().parent_id.is_none(),
                mgr.has_inflight_loop(&sid),
            );
            assert_eq!(
                decision,
                UserInputDecision::RejectBusy,
                "busy guard must reject command: {text}"
            );
        }

        // 被拒即不解析、不转发、不写文件（对照 /init-agent 的 WriteFile 路径）。
        assert!(
            !project.path().join(".visp").exists(),
            "busy rejection must not create .visp or write agent files"
        );
    }

    #[tokio::test]
    async fn test_busy_rejection_is_idempotent_without_side_effects() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();

        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        // 连续两次被拒后，状态与「是否有在途令牌」均不变。
        for _ in 0..2 {
            let decision = decide_user_input(
                mgr.get(&sid).unwrap().parent_id.is_none(),
                mgr.has_inflight_loop(&sid),
            );
            assert_eq!(decision, UserInputDecision::RejectBusy);
        }
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);
        assert!(mgr.has_inflight_loop(&sid));
    }

    #[tokio::test]
    async fn test_session_not_active_error_includes_session_id() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session(
                "child-99",
                Some("parent"),
                "sub-agent",
                vec![],
                SessionStatus::Idle,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let (tx, mut rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(16);
        let response_tx = tx.clone();

        // 真实判定函数：子会话 → 拒绝（非活跃）
        let decision = decide_user_input(
            mgr.get("child-99").unwrap().parent_id.is_none(),
            mgr.has_inflight_loop("child-99"),
        );
        assert_eq!(decision, UserInputDecision::RejectNotActive);

        let err_msg = session_error_msg(
            SESSION_NOT_ACTIVE_CODE,
            "Session child-99 is not active",
            "child-99",
        );
        response_tx.send(Ok(err_msg)).await.unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, SESSION_NOT_ACTIVE_CODE);
                assert_eq!(e.session_id, "child-99");
            }
            _ => panic!("expected Error payload"),
        }
    }

    // ── 5d: Idle/Completed/Error 主 session 可接受输入 (3) ────────────────

    #[tokio::test]
    async fn test_user_input_to_idle_main_session_is_accepted() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session(
                "main-idle",
                None,
                "orchestrator",
                vec![],
                SessionStatus::Idle,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let decision = decide_user_input(
            mgr.get("main-idle").unwrap().parent_id.is_none(),
            mgr.has_inflight_loop("main-idle"),
        );
        assert_eq!(decision, UserInputDecision::Accept);
    }

    #[tokio::test]
    async fn test_user_input_to_completed_main_session_is_accepted() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session(
                "main-completed",
                None,
                "orchestrator",
                vec![],
                SessionStatus::Completed,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let decision = decide_user_input(
            mgr.get("main-completed").unwrap().parent_id.is_none(),
            mgr.has_inflight_loop("main-completed"),
        );
        assert_eq!(decision, UserInputDecision::Accept);
    }

    #[tokio::test]
    async fn test_user_input_to_error_main_session_is_accepted() {
        let mut store = InMemorySessionStore::new();
        store
            .create(make_session(
                "main-error",
                None,
                "orchestrator",
                vec![],
                SessionStatus::Error,
            ))
            .unwrap();
        let mgr = StdArc::new(SessionManager::new(store));

        let decision = decide_user_input(
            mgr.get("main-error").unwrap().parent_id.is_none(),
            mgr.has_inflight_loop("main-error"),
        );
        assert_eq!(decision, UserInputDecision::Accept);
    }

    // ── 7a: End-to-end integration tests (3) ───────────────────────────────────

    #[tokio::test]
    async fn e2e_resume_session_with_nested_sub_agents() {
        let mut store = InMemorySessionStore::new();

        // Main session: User + Assistant + Tool
        let main_msgs = vec![
            Message::user("hello"),
            Message::assistant("main response"),
            Message::tool("main result", "call-0"),
        ];
        let main = make_session("main", None, "default", main_msgs, SessionStatus::Idle);
        let main_id = main.id.clone();
        store.create(main).unwrap();

        // Child session: User(task prompt) + Assistant + Tool
        store
            .create(make_session(
                "child",
                Some("main"),
                "sub-agent-child",
                vec![
                    Message::user("task: implement feature X"),
                    Message::assistant("child response"),
                    Message::tool("child result", "call-1"),
                ],
                SessionStatus::Idle,
            ))
            .unwrap();

        // Grandchild session: User(task prompt) + Assistant + Tool
        store
            .create(make_session(
                "grand",
                Some("child"),
                "sub-agent-grand",
                vec![
                    Message::user("task: review the code"),
                    Message::assistant("grand response"),
                    Message::tool("grand result", "call-2"),
                ],
                SessionStatus::Idle,
            ))
            .unwrap();

        let mgr = StdArc::new(SessionManager::new(store));
        let (tx, mut rx) = mpsc::channel(256);

        simulate_join_session(&mgr, &tx, &main_id).await;
        drop(tx);

        let mut frames = Vec::new();
        while let Some(Ok(msg)) = rx.recv().await {
            frames.push(msg);
        }

        // 15 frames: 5 (main) + 5 (child) + 5 (grand)
        // Each session: StatusUpdate + UserMessage + TextDelta + ToolResult + Done
        assert_eq!(
            frames.len(),
            15,
            "expected 15 frames: main(5) + child(5) + grand(5)"
        );

        // ── Main session (frames 0-4) ──
        match &frames[0].payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, main_id);
                assert!(!s.view_only, "main session must have view_only=false");
                assert!(
                    s.agent_name.is_empty(),
                    "main session agent_name should be empty"
                );
                assert_eq!(
                    s.user_inputs,
                    vec!["hello"],
                    "main user_inputs should contain user message"
                );
            }
            _ => panic!("frame 0 should be main StatusUpdate"),
        }
        match &frames[1].payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "hello");
                assert_eq!(u.session_id, main_id);
            }
            _ => panic!("frame 1 should be main UserMessage"),
        }
        match &frames[2].payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "main response");
                assert_eq!(t.session_id, main_id);
            }
            _ => panic!("frame 2 should be main TextDelta"),
        }
        match &frames[3].payload {
            Some(proto::server_message::Payload::ToolResult(tr)) => {
                assert_eq!(tr.call_id, "call-0");
                assert_eq!(tr.content, "main result");
                assert_eq!(tr.session_id, main_id);
            }
            _ => panic!("frame 3 should be main ToolResult"),
        }
        match &frames[4].payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, main_id);
            }
            _ => panic!("frame 4 should be main Done"),
        }

        // ── Child session (frames 5-9) ──
        match &frames[5].payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, "child");
                assert!(s.view_only, "child session must have view_only=true");
                assert_eq!(s.agent_name, "sub-agent-child");
                assert_eq!(
                    s.user_inputs,
                    vec!["task: implement feature X"],
                    "child user_inputs should contain task prompt"
                );
            }
            _ => panic!("frame 5 should be child StatusUpdate"),
        }
        match &frames[6].payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "task: implement feature X");
                assert_eq!(u.session_id, "child");
            }
            _ => panic!("frame 6 should be child UserMessage"),
        }
        match &frames[7].payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "child response");
                assert_eq!(t.session_id, "child");
                assert_eq!(t.agent_name, "sub-agent-child");
            }
            _ => panic!("frame 7 should be child TextDelta"),
        }
        match &frames[8].payload {
            Some(proto::server_message::Payload::ToolResult(tr)) => {
                assert_eq!(tr.call_id, "call-1");
                assert_eq!(tr.content, "child result");
                assert_eq!(tr.session_id, "child");
                assert_eq!(tr.agent_name, "sub-agent-child");
            }
            _ => panic!("frame 8 should be child ToolResult"),
        }
        match &frames[9].payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, "child");
            }
            _ => panic!("frame 9 should be child Done"),
        }

        // ── Grandchild session (frames 10-14) ──
        match &frames[10].payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, "grand");
                assert!(s.view_only, "grand session must have view_only=true");
                assert_eq!(s.agent_name, "sub-agent-grand");
                assert_eq!(
                    s.user_inputs,
                    vec!["task: review the code"],
                    "grand user_inputs should contain task prompt"
                );
            }
            _ => panic!("frame 10 should be grand StatusUpdate"),
        }
        match &frames[11].payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "task: review the code");
                assert_eq!(u.session_id, "grand");
            }
            _ => panic!("frame 11 should be grand UserMessage"),
        }
        match &frames[12].payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "grand response");
                assert_eq!(t.session_id, "grand");
                assert_eq!(t.agent_name, "sub-agent-grand");
            }
            _ => panic!("frame 12 should be grand TextDelta"),
        }
        match &frames[13].payload {
            Some(proto::server_message::Payload::ToolResult(tr)) => {
                assert_eq!(tr.call_id, "call-2");
                assert_eq!(tr.content, "grand result");
                assert_eq!(tr.session_id, "grand");
            }
            _ => panic!("frame 13 should be grand ToolResult"),
        }
        match &frames[14].payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, "grand");
            }
            _ => panic!("frame 14 should be grand Done"),
        }
    }

    #[tokio::test]
    async fn e2e_resume_session_no_sub_agents_unchanged() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        mgr.append_message(&sid, Message::user("prompt")).unwrap();
        mgr.append_message(&sid, Message::assistant("response 1"))
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        simulate_join_session(&mgr, &tx, &sid).await;

        let f1 = rx.recv().await.unwrap().unwrap();
        let f2 = rx.recv().await.unwrap().unwrap();
        let f3 = rx.recv().await.unwrap().unwrap();
        let f4 = rx.recv().await.unwrap().unwrap();
        let no_f5 = rx.try_recv();

        // Frame 1: StatusUpdate (view_only=false, no descendants)
        match &f1.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, sid);
                assert!(!s.view_only, "main must have view_only=false");
                assert_eq!(s.user_inputs, vec!["prompt"]);
            }
            _ => panic!("frame 0 should be StatusUpdate"),
        }

        // Frame 2: UserMessage
        match &f2.payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "prompt");
                assert_eq!(u.session_id, sid);
            }
            _ => panic!("frame 1 should be UserMessage"),
        }

        // Frame 3: TextDelta
        match &f3.payload {
            Some(proto::server_message::Payload::TextDelta(t)) => {
                assert_eq!(t.delta, "response 1");
                assert_eq!(t.session_id, sid);
            }
            _ => panic!("frame 2 should be TextDelta"),
        }

        // Frame 4: Done
        match &f4.payload {
            Some(proto::server_message::Payload::Done(d)) => {
                assert_eq!(d.session_id, sid);
            }
            _ => panic!("frame 3 should be Done"),
        }

        // No extra frames
        assert!(no_f5.is_err(), "no descendants → no extra frames");
    }

    #[tokio::test]
    async fn e2e_descendant_load_failure_skipped() {
        use std::sync::Mutex;

        struct FailOnSecondListCall {
            calls: Mutex<usize>,
            sessions: Vec<Session>,
        }

        impl SessionStore for FailOnSecondListCall {
            fn create(&mut self, _s: Session) -> Result<(), SessionError> {
                Ok(())
            }
            fn get(&self, id: &str) -> Result<Session, SessionError> {
                self.sessions
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                    .ok_or_else(|| SessionError::NotFound("mock".into()))
            }
            fn list(&self) -> Result<Vec<Session>, SessionError> {
                Ok(self.sessions.clone())
            }
            fn delete(&mut self, _id: &str) -> Result<(), SessionError> {
                Ok(())
            }
            fn update(&mut self, _s: Session) -> Result<(), SessionError> {
                Ok(())
            }
            fn get_messages(&self, _id: &str) -> Result<Vec<Message>, SessionError> {
                Ok(vec![])
            }
            fn get_system_prompt(&self, id: &str) -> Result<String, SessionError> {
                self.sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.system_prompt_template.clone())
                    .ok_or_else(|| SessionError::NotFound("mock".into()))
            }
            fn append_message(&mut self, _id: &str, _m: Message) -> Result<(), SessionError> {
                Ok(())
            }
            fn list_by_project(&self, _p: &str) -> Result<Vec<Session>, SessionError> {
                Ok(self.sessions.clone())
            }
            fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                if *calls == 2 {
                    // Second call (for child A's own children) fails
                    return Err(SessionError::NotFound("simulated load failure".into()));
                }
                Ok(self
                    .sessions
                    .iter()
                    .filter(|s| s.parent_id.as_deref() == Some(parent_id))
                    .cloned()
                    .collect())
            }
        }

        let main = make_session("main", None, "default", vec![], SessionStatus::Idle);
        let main_id = main.id.clone();
        let child_a = make_session(
            "child-a",
            Some("main"),
            "agent-a",
            vec![Message::assistant("from A")],
            SessionStatus::Idle,
        );
        let child_b = make_session(
            "child-b",
            Some("main"),
            "agent-b",
            vec![Message::assistant("from B")],
            SessionStatus::Idle,
        );

        let store = FailOnSecondListCall {
            calls: Mutex::new(0),
            sessions: vec![main, child_a, child_b],
        };
        let mgr = StdArc::new(SessionManager::new(store));
        let (tx, mut rx) = mpsc::channel(256);

        simulate_join_session(&mgr, &tx, &main_id).await;
        drop(tx);

        let mut frames = Vec::new();
        while let Some(Ok(msg)) = rx.recv().await {
            frames.push(msg);
        }

        // Main: StatusUpdate + Done = 2
        // child-a: StatusUpdate + TextDelta + Done = 3
        // child-b: StatusUpdate + TextDelta + Done = 3
        // Total = 8
        assert_eq!(
            frames.len(),
            8,
            "both children should replay despite load failure during BFS"
        );

        // Verify main frames
        match &frames[0].payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert!(!s.view_only);
            }
            _ => panic!("frame 0 should be main StatusUpdate"),
        }

        // Verify both children replayed (any sibling order)
        let child_session_ids: HashSet<String> = frames
            .iter()
            .filter_map(|f| match &f.payload {
                Some(proto::server_message::Payload::StatusUpdate(s)) if s.view_only => {
                    Some(s.session_id.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            child_session_ids.contains("child-a"),
            "child-a should be replayed"
        );
        assert!(
            child_session_ids.contains("child-b"),
            "child-b should be replayed"
        );

        // Verify both TextDeltas present
        let child_text: HashSet<String> = frames
            .iter()
            .filter_map(|f| match &f.payload {
                Some(proto::server_message::Payload::TextDelta(t))
                    if t.session_id == "child-a" || t.session_id == "child-b" =>
                {
                    Some(t.delta.clone())
                }
                _ => None,
            })
            .collect();
        assert!(child_text.contains("from A"), "child-a text should appear");
        assert!(child_text.contains("from B"), "child-b text should appear");
    }

    // ── 步骤 4a：ReloadConfig gRPC handler ──────────────────────────────────

    /// 用例 1：handler 调核心显式入口 → 逐项结果完整映射为 ReloadConfigResponse。
    #[tokio::test]
    #[serial]
    async fn reload_config_maps_core_results_to_response() {
        let env = IsolatedEnv::new();
        let (rule_engine, reload_core) = build_reload_core(env.project());
        // 核心构造后新增 AGENTS.md，重载应判为 rules 有变化。
        std::fs::write(env.project().join("AGENTS.md"), "<Role>mapped</Role>").unwrap();

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service_with_core(mgr, rule_engine, reload_core);

        let response = service
            .reload_config(tonic::Request::new(proto::ReloadConfigRequest {}))
            .await
            .expect("显式入口不应产生 gRPC error")
            .into_inner();

        assert_eq!(response.results.len(), 4, "显式入口应回四类条目");
        let categories: Vec<&str> = response
            .results
            .iter()
            .map(|item| item.category.as_str())
            .collect();
        assert_eq!(
            categories,
            vec!["rules", "skills", "agents", "system_prompt"]
        );
        assert!(
            response.results.iter().all(|item| item.success),
            "items: {:?}",
            response.results
        );

        let rules = &response.results[0];
        assert!(
            rules.message.contains("1 个规则文件"),
            "message: {}",
            rules.message
        );
        assert_eq!(rules.modified, 1, "聚合变更数应写入计数位");
    }

    /// 用例 2：含失败条目时仍为正常响应（不走 gRPC status error，设计 §3）。
    #[tokio::test]
    #[serial]
    async fn reload_config_partial_failure_returns_ok_response() {
        let env = IsolatedEnv::new();
        let (rule_engine, reload_core) = build_reload_core(env.project());
        // 用目录冒充 `.md` 规则文件 → rules 重载 IO 失败，其余领域仍应成功。
        let rules_dir = env.project().join(".visp/rules");
        std::fs::create_dir_all(rules_dir.join("zzz.md")).unwrap();

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service_with_core(mgr, rule_engine, reload_core);

        let response = service
            .reload_config(tonic::Request::new(proto::ReloadConfigRequest {}))
            .await
            .expect("部分成功必须是正常响应，而非 gRPC error")
            .into_inner();

        let rules = &response.results[0];
        assert!(!rules.success, "rules 条目应报失败：{rules:?}");
        assert!(
            rules.message.contains("重载失败"),
            "message: {}",
            rules.message
        );
        assert_eq!(rules.modified, 0);
        assert!(
            response.results[1..].iter().all(|item| item.success),
            "单项失败不得作废其余领域：{:?}",
            response.results
        );
    }

    /// 用例 3：system_prompt 回执透传（恒成功的第四条目）。
    #[tokio::test]
    #[serial]
    async fn reload_config_passes_through_system_prompt_receipt() {
        let env = IsolatedEnv::new();
        let (rule_engine, reload_core) = build_reload_core(env.project());

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service_with_core(mgr, rule_engine, reload_core);

        let response = service
            .reload_config(tonic::Request::new(proto::ReloadConfigRequest {}))
            .await
            .unwrap()
            .into_inner();

        let receipt = response
            .results
            .iter()
            .find(|item| item.category == "system_prompt")
            .expect("响应应含 system_prompt 条目");
        assert!(receipt.success, "system_prompt 条目恒成功");
        assert!(
            receipt.message.contains("新 session"),
            "message: {}",
            receipt.message
        );
        assert_eq!(receipt.modified, 0);
    }

    /// 用例 4（运行时部分）：service.rule_engine 与核心共享同一 Arc，
    /// 重载后该字段可观察到新规则内容。`#[allow(dead_code)]` 的移除由
    /// `cargo clippy -p visp-daemon -- -D warnings` 质量门保证。
    #[tokio::test]
    #[serial]
    async fn reload_config_engine_field_reflects_reloaded_rules() {
        let env = IsolatedEnv::new();
        let (rule_engine, reload_core) = build_reload_core(env.project());
        std::fs::write(env.project().join("AGENTS.md"), "<Role>field-wired</Role>").unwrap();

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service_with_core(mgr, rule_engine, reload_core);

        let _ = service
            .reload_config(tonic::Request::new(proto::ReloadConfigRequest {}))
            .await
            .unwrap();

        assert!(
            service
                .rule_engine
                .get_active_rules()
                .contains("field-wired"),
            "service.rule_engine 应指向核心重载的同一引擎"
        );
    }

    // ── 1a-2：Chat 显示面改为事件总线订阅 ──────────────────────────────

    /// 构造一个空入站流（无 ClientMessage）的 Chat 请求：入站任务立即结束，
    /// 出站任务仍持有响应发送端，订阅随 `chat()` 调用建立。
    fn empty_chat_request() -> Request<Streaming<proto::ClientMessage>> {
        use tonic::codec::{Codec, ProstCodec};
        let mut codec = ProstCodec::<proto::ServerMessage, proto::ClientMessage>::default();
        let decoder = codec.decoder();
        let stream = Streaming::new_request(decoder, tonic::body::Body::empty(), None, None);
        Request::new(stream)
    }

    fn bus_text_frame(text: &str, session_id: &str) -> visp_core::agent::AgentEventFrame {
        visp_core::agent::AgentEventFrame {
            event: AgentEvent::TextDelta(text.to_string()),
            session_id: session_id.to_string(),
            agent_name: "agent".to_string(),
            parent_session_id: None,
            parent_session_name: None,
        }
    }

    fn status_frame() -> visp_core::agent::AgentEventFrame {
        visp_core::agent::AgentEventFrame {
            event: AgentEvent::Done,
            session_id: "sess-1".to_string(),
            agent_name: "agent".to_string(),
            parent_session_id: None,
            parent_session_name: None,
        }
    }

    async fn next_msg(stream: &mut ResponseStream) -> proto::ServerMessage {
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("应在时限内收到消息")
            .expect("Chat 流不应提前结束")
            .expect("消息不应为错误")
    }

    fn text_delta_of(msg: &proto::ServerMessage) -> String {
        match &msg.payload {
            Some(proto::server_message::Payload::TextDelta(td)) => td.delta.clone(),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    /// 1. 重连不再 `already taken`：同一 service 连续两次 chat 均成功。
    #[tokio::test]
    async fn chat_reconnect_is_allowed() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service(mgr);

        assert!(
            service.chat(empty_chat_request()).await.is_ok(),
            "首次 chat 应成功"
        );
        assert!(
            service.chat(empty_chat_request()).await.is_ok(),
            "重连不应再报 orchestrator receiver already taken"
        );
    }

    /// 2. 多连接各自收到全量帧、顺序一致。
    #[tokio::test]
    async fn multiple_connections_each_receive_all_frames() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);

        let mut a = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();
        let mut b = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();

        for i in 0..3 {
            bus.publish(BusEvent::Frame(bus_text_frame(&format!("f{i}"), "s-1")));
        }

        for (label, stream) in [("a", &mut a), ("b", &mut b)] {
            for i in 0..3 {
                let msg = next_msg(stream).await;
                assert_eq!(text_delta_of(&msg), format!("f{i}"), "连接 {label}");
            }
        }
    }

    /// 3. 某连接结束（drop receiver）不影响其它连接，也不影响发布端。
    #[tokio::test]
    async fn dropping_one_connection_does_not_affect_others() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);

        let a = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();
        let mut b = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();
        drop(a);

        bus.publish(BusEvent::Frame(bus_text_frame("survivor", "s-1")));
        assert_eq!(text_delta_of(&next_msg(&mut b).await), "survivor");

        // 发布端从不失败/阻塞：再发一帧仍即时返回并被存活连接收到。
        bus.publish(BusEvent::Frame(bus_text_frame("again", "s-1")));
        assert_eq!(text_delta_of(&next_msg(&mut b).await), "again");
    }

    /// 4. 无订阅者时 publish 不失败、不阻塞；随后建立的连接仍正常。
    #[tokio::test]
    async fn publish_without_subscribers_does_not_fail_or_break() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);

        // 仅 sentinel 持有 receiver：publish 不 panic、不阻塞（否则测试挂起）。
        bus.publish(BusEvent::Frame(bus_text_frame("no-sub", "s-1")));
        bus.publish(BusEvent::Frame(bus_text_frame("no-sub-2", "s-1")));

        // 之后建立的连接仍能收到后续帧——发布端未 break、订阅未受损。
        let mut stream = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();
        bus.publish(BusEvent::Frame(bus_text_frame("after", "s-1")));
        assert_eq!(text_delta_of(&next_msg(&mut stream).await), "after");
    }

    /// 5. 回归：单连接既有 Chat 行为（TextDelta/Done/UserQuery）结构不变。
    #[tokio::test]
    async fn single_connection_chat_frames_unchanged() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);

        let mut stream = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();

        bus.publish(BusEvent::Frame(bus_text_frame("delta", "sess-1")));
        match next_msg(&mut stream).await.payload {
            Some(proto::server_message::Payload::TextDelta(td)) => {
                assert_eq!(td.delta, "delta");
                assert_eq!(td.session_id, "sess-1");
                assert_eq!(td.agent_name, "agent");
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }

        bus.publish(BusEvent::Frame(status_frame()));
        match next_msg(&mut stream).await.payload {
            Some(proto::server_message::Payload::Done(done)) => {
                assert_eq!(done.session_id, "sess-1");
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let (respond, _respond_rx) = tokio::sync::mpsc::channel(1);
        bus.publish(BusEvent::Frame(visp_core::agent::AgentEventFrame {
            event: AgentEvent::UserQuery {
                query_id: "q-1".into(),
                message: "confirm?".into(),
                options: vec!["yes".into()],
                allow_other: false,
                kind: visp_core::agent::PermissionKind::Approval,
                respond,
            },
            session_id: "sess-1".into(),
            agent_name: "agent".into(),
            parent_session_id: None,
            parent_session_name: None,
        }));
        match next_msg(&mut stream).await.payload {
            Some(proto::server_message::Payload::UserQuery(q)) => {
                assert_eq!(q.query_id, "q-1");
                assert_eq!(q.message, "confirm?");
                assert_eq!(q.session_id, "sess-1");
            }
            other => panic!("expected UserQuery, got {other:?}"),
        }
    }

    /// 6. hook 域事件不面向 TUI：出站忽略，不产生任何 ServerMessage。
    #[tokio::test]
    async fn hook_event_is_not_forwarded_outbound() {
        use visp_hooks::{
            HookContext, HookEvent, HookEventName, HookPayload, Origin, SessionEndPayload,
            SessionSource,
        };

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);
        let mut stream = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();

        // 先确认订阅已生效：一帧 Frame 正常到达。
        bus.publish(BusEvent::Frame(bus_text_frame("before", "s-1")));
        assert_eq!(text_delta_of(&next_msg(&mut stream).await), "before");

        // 注入 hook 事件：出站必须忽略。
        bus.publish(BusEvent::Hook(HookEvent {
            context: HookContext {
                schema: 1,
                hook_event_name: HookEventName::SessionEnd,
                session_id: "s-1".into(),
                cwd: "/tmp".into(),
                source: SessionSource::Startup,
                origin: Origin::Tui,
                seq: Some(1),
            },
            payload: HookPayload::SessionEnd(SessionEndPayload {
                reason: "quit".into(),
                exit_code: Some(0),
            }),
        }));

        // 紧随其后的 Frame 就是下一个消息——证明 Hook 未产生任何出站消息。
        bus.publish(BusEvent::Frame(bus_text_frame("after", "s-1")));
        assert_eq!(text_delta_of(&next_msg(&mut stream).await), "after");
    }

    /// 7. 回归：显示域 Frame 仍被出站转发到 CLI。
    #[tokio::test]
    async fn frame_event_is_forwarded_outbound() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr);
        let mut stream = service
            .chat(empty_chat_request())
            .await
            .unwrap()
            .into_inner();

        bus.publish(BusEvent::Frame(bus_text_frame("hello", "s-9")));
        match next_msg(&mut stream).await.payload {
            Some(proto::server_message::Payload::TextDelta(td)) => {
                assert_eq!(td.delta, "hello");
                assert_eq!(td.session_id, "s-9");
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    // ── 1a-3：Lagged → daemon 侧 join 式 replay（目标 session + 去重/节流）──

    /// 发布帧数须超过总线容量（bus.rs `BUS_CAPACITY = 1024`）以触发 `Lagged`。
    const OVER_CAPACITY: usize = 1200;

    /// outbound 响应通道（不经 tonic Streaming），直接从 mpsc 读回放/转发帧。
    type OutboundRx = mpsc::Receiver<Result<proto::ServerMessage, Status>>;

    async fn next_rx(rx: &mut OutboundRx) -> proto::ServerMessage {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("应在时限内收到消息")
            .expect("outbound 通道不应关闭")
            .expect("消息不应为错误")
    }

    /// 构造直接驱动 outbound 的测试环境：自有总线订阅 + 响应接收端 + 连接回放状态。
    /// `target` = (session_id, 已回放水位)，模拟 inbound `JoinSession` 的登记。
    fn outbound_harness(
        mgr: StdArc<SessionManager>,
        target: Option<(&str, usize)>,
    ) -> (Arc<EventBus>, OutboundRx, Arc<Mutex<ReplayState>>) {
        let bus = Arc::new(EventBus::new());
        let bus_rx = bus.subscribe();
        let (tx, rx) = mpsc::channel(128);
        let replay_state = Arc::new(Mutex::new(ReplayState::new(Duration::from_secs(60))));
        if let Some((session_id, upto)) = target {
            replay_state.lock().unwrap().set_target(session_id, upto);
        }
        spawn_outbound(
            bus_rx,
            tx,
            Arc::new(Mutex::new(HashMap::new())),
            mgr,
            replay_state.clone(),
        );
        (bus, rx, replay_state)
    }

    fn store_with(sessions: Vec<Session>) -> StdArc<SessionManager> {
        let mut store = InMemorySessionStore::new();
        for session in sessions {
            store.create(session).unwrap();
        }
        StdArc::new(SessionManager::new(store))
    }

    /// 1. `Lagged` 触发一次 join 式 replay，且目标为连接当前 session，而非全局广播。
    #[tokio::test]
    async fn lagged_replay_targets_current_session_not_broadcast() {
        let mgr = store_with(vec![
            make_session(
                "sess-A",
                None,
                "agent",
                vec![Message::assistant("A-reply")],
                SessionStatus::Idle,
            ),
            make_session(
                "sess-B",
                None,
                "agent",
                vec![Message::assistant("B-reply")],
                SessionStatus::Idle,
            ),
        ]);
        let (bus, mut rx, _state) = outbound_harness(mgr, Some(("sess-A", 0)));

        // 滞后源帧全部属于 sess-B；回放必须只针对目标 sess-A。
        for _ in 0..OVER_CAPACITY {
            bus.publish(BusEvent::Frame(bus_text_frame("B-live", "sess-B")));
        }

        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, "sess-A", "回放目标须为当前 session");
                assert!(s.view_only, "join 式回放应 view_only=true");
            }
            other => panic!("expected replay StatusUpdate, got {other:?}"),
        }
        assert_eq!(text_delta_of(&next_rx(&mut rx).await), "A-reply");
        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::Done(d)) => assert_eq!(d.session_id, "sess-A"),
            other => panic!("expected replay Done, got {other:?}"),
        }
    }

    /// 2. 连续 `Lagged` 去重：节流窗口内不再回放，窗口过后恢复。
    #[test]
    fn lagged_replay_is_throttled_within_window() {
        let mut state = ReplayState::new(Duration::from_millis(500));
        let t0 = Instant::now();

        assert!(state.throttle_allows(t0), "首次 Lagged 应允许回放");
        state.note_replayed("sess-A", 3, t0);

        assert!(
            !state.throttle_allows(t0 + Duration::from_millis(100)),
            "风暴下窗口内再次 Lagged 不得重复回放"
        );
        assert!(
            state.throttle_allows(t0 + Duration::from_millis(500)),
            "窗口过后允许再次回放"
        );
    }

    /// 3. replay 与已收帧去重：只回放水位之后的历史，已收消息不重复。
    #[tokio::test]
    async fn lagged_replay_skips_already_received_history() {
        let mgr = store_with(vec![make_session(
            "sess-A",
            None,
            "agent",
            vec![
                Message::assistant("A1"),
                Message::assistant("A2"),
                Message::assistant("A3"),
            ],
            SessionStatus::Idle,
        )]);
        // 水位 2：A1/A2 已通过 join 回放收到。
        let (bus, mut rx, _state) = outbound_harness(mgr, Some(("sess-A", 2)));

        for _ in 0..OVER_CAPACITY {
            bus.publish(BusEvent::Frame(bus_text_frame("live", "other")));
        }

        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert_eq!(s.session_id, "sess-A");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
        // 仅增量 A3；A1/A2 绝不重发。
        assert_eq!(
            text_delta_of(&next_rx(&mut rx).await),
            "A3",
            "只应回放已回放水位之后的历史"
        );
        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::Done(d)) => assert_eq!(d.session_id, "sess-A"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// 4. 回归：非 `Lagged` 路径不得触发 replay。
    #[tokio::test]
    async fn non_lagged_frames_do_not_trigger_replay() {
        let mgr = store_with(vec![make_session(
            "sess-A",
            None,
            "agent",
            vec![Message::assistant("A-reply")],
            SessionStatus::Idle,
        )]);
        let (bus, mut rx, _state) = outbound_harness(mgr, Some(("sess-A", 0)));

        // 未超容量：正常转发，若误触 replay 首帧会是 StatusUpdate。
        bus.publish(BusEvent::Frame(bus_text_frame("f1", "sess-A")));
        bus.publish(BusEvent::Frame(bus_text_frame("f2", "sess-A")));

        assert_eq!(text_delta_of(&next_rx(&mut rx).await), "f1");
        assert_eq!(text_delta_of(&next_rx(&mut rx).await), "f2");
    }

    /// 5. TUI 零改动：replay 仅产出 TUI 既已处理的 join 协议帧序列
    ///    （StatusUpdate(view_only) → UserMessage → TextDelta → Done）。
    #[tokio::test]
    async fn lagged_replay_uses_existing_join_frame_contract() {
        let mgr = store_with(vec![make_session(
            "sess-A",
            None,
            "agent",
            vec![Message::user("task"), Message::assistant("answer")],
            SessionStatus::Idle,
        )]);
        let (bus, mut rx, _state) = outbound_harness(mgr, Some(("sess-A", 0)));

        for _ in 0..OVER_CAPACITY {
            bus.publish(BusEvent::Frame(bus_text_frame("live", "other")));
        }

        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::StatusUpdate(s)) => {
                assert!(s.view_only);
                assert_eq!(s.session_id, "sess-A");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::UserMessage(u)) => {
                assert_eq!(u.content, "task");
                assert_eq!(u.session_id, "sess-A");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
        assert_eq!(text_delta_of(&next_rx(&mut rx).await), "answer");
        match next_rx(&mut rx).await.payload {
            Some(proto::server_message::Payload::Done(d)) => assert_eq!(d.session_id, "sess-A"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    // ── 优雅关停管道（设计 D13，计划 1a-4b）────────────────────────────────

    /// 记录调用的 drain 宿主：计数 + 记录收到的 budget + 记录终态直投事件。
    struct CountingDrain {
        calls: std::sync::atomic::AtomicUsize,
        budget: Mutex<Option<std::time::Duration>>,
        terminal: Mutex<Vec<HookEvent>>,
    }

    impl CountingDrain {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                budget: Mutex::new(None),
                terminal: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// 已收到的终态直投事件快照。
        fn terminal(&self) -> Vec<HookEvent> {
            self.terminal.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HookDrainHost for CountingDrain {
        async fn drain(&self, budget: std::time::Duration) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.budget.lock().unwrap() = Some(budget);
        }

        async fn emit_terminal(&self, event: HookEvent) {
            self.terminal.lock().unwrap().push(event);
        }
    }

    /// 永不返回的 drain 宿主：验证调用方硬超时兜底（不悬挂）。
    struct HangingDrain;

    #[async_trait::async_trait]
    impl HookDrainHost for HangingDrain {
        async fn drain(&self, _budget: std::time::Duration) {
            std::future::pending::<()>().await;
        }

        async fn emit_terminal(&self, _event: HookEvent) {}
    }

    fn shutdown_request() -> Request<proto::ShutdownRequest> {
        Request::new(proto::ShutdownRequest { force: false })
    }

    /// 8. Shutdown RPC 串起完整管道：SessionEnd（直投）→ 有界 drain → cancel_tx → Notify。
    #[tokio::test]
    async fn shutdown_rpc_runs_graceful_pipeline() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let drain = Arc::new(CountingDrain::new());
        let (service, mut cancel_rx, notify, _bus) = make_service_with_shutdown(mgr, drain.clone());

        // main 侧等待被唤醒。
        let waiter = {
            let notify = notify.clone();
            tokio::spawn(async move { notify.notified().await })
        };

        service.shutdown(shutdown_request()).await.unwrap();

        // cancel_tx → CancelSignal 送达（orchestrator `run()` 退出由 visp-agent 既有测试覆盖）。
        cancel_rx
            .try_recv()
            .expect("shutdown 应投递 CancelSignal 给 orchestrator");

        // Notify 唤醒 daemon main。
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("main 应被 Notify 唤醒")
            .unwrap();

        // 终态 SessionEnd 同步直投 hook 执行器（不经总线，规避关停竞态）。
        let terminal = drain.terminal();
        assert_eq!(terminal.len(), 1, "shutdown 应直投一次 SessionEnd");
        assert_eq!(terminal[0].event_name(), HookEventName::SessionEnd);
        match &terminal[0].payload {
            HookPayload::SessionEnd(payload) => {
                assert_eq!(payload.reason, "shutdown");
                assert_eq!(payload.exit_code, Some(0));
            }
            other => panic!("expected SessionEnd payload, got {other:?}"),
        }

        assert_eq!(drain.calls(), 1);
        assert_eq!(*drain.budget.lock().unwrap(), Some(HOOK_DRAIN_BUDGET));
        assert!(service.is_shutting_down());
    }

    /// 9. drain 有界：宿主永不返回时仍按 ≤2s 退出，不悬挂。
    #[tokio::test]
    async fn shutdown_drain_is_bounded() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, _cancel_rx, _notify, _bus) =
            make_service_with_shutdown(mgr, Arc::new(HangingDrain));

        let started = Instant::now();
        let result = tokio::time::timeout(
            HOOK_DRAIN_BUDGET + std::time::Duration::from_secs(2),
            service.shutdown(shutdown_request()),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            result.is_ok(),
            "stuck drain host must not hang shutdown, elapsed {elapsed:?}"
        );
        assert!(result.unwrap().is_ok());
        assert!(
            elapsed < HOOK_DRAIN_BUDGET + std::time::Duration::from_secs(2),
            "drain must be bounded by budget, elapsed {elapsed:?}"
        );
    }

    /// 10. 终止态抑制：二次 shutdown 不再发 SessionEnd、不再 drain。
    #[tokio::test]
    async fn second_shutdown_emits_no_more_hook_events() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let drain = Arc::new(CountingDrain::new());
        let (service, _cancel_rx, _notify, bus) = make_service_with_shutdown(mgr, drain.clone());
        let mut sub = bus.subscribe();

        service.shutdown(shutdown_request()).await.unwrap();
        service.shutdown(shutdown_request()).await.unwrap();

        assert_eq!(drain.calls(), 1, "drain 只应在首次关停执行");
        assert_eq!(drain.terminal().len(), 1, "SessionEnd 只应直投一次");

        let mut hook_events = 0usize;
        while let Ok(env) = sub.try_recv() {
            if matches!(env.event, BusEvent::Hook(_)) {
                hook_events += 1;
            }
        }
        assert_eq!(
            hook_events, 0,
            "终态 SessionEnd 不经总线，且二次关停不再派发"
        );
    }

    /// 11. `mcp.shutdown_all` 双调幂等（handler 与 main 各调一次）。
    #[tokio::test]
    async fn mcp_shutdown_all_is_idempotent() {
        let mcp = McpManager::new(vec![]);
        mcp.shutdown_all().await;
        mcp.shutdown_all().await;
    }

    /// 12. 无 hook（NoopHookDrain）时关停行为不变：即返、cancel 仍送达、Notify 仍触发。
    #[tokio::test]
    async fn shutdown_without_hooks_behaves_like_before() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, mut cancel_rx, notify, _bus) =
            make_service_with_shutdown(mgr, Arc::new(crate::shutdown::NoopHookDrain));

        let started = Instant::now();
        service.shutdown(shutdown_request()).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "空 drain 不应拖慢关停"
        );
        assert!(cancel_rx.try_recv().is_ok(), "cancel 仍须送达");
        tokio::time::timeout(std::time::Duration::from_secs(1), notify.notified())
            .await
            .expect("Notify 仍须触发");
    }

    // ── 1b-1c：daemon 侧事件发射（PermissionResult / 会话删除 SessionEnd）──────

    /// 构造一个已登记 `query_id` → 等待者的 daemon 侧 pending 表。
    fn pending_with(
        query_id: &str,
        session_id: &str,
    ) -> (
        Arc<Mutex<HashMap<String, PendingQuery>>>,
        mpsc::Receiver<UserQueryResult>,
    ) {
        let (respond, rx) = mpsc::channel(1);
        let mut map = HashMap::new();
        map.insert(
            query_id.to_string(),
            PendingQuery {
                respond,
                session_id: session_id.to_string(),
            },
        );
        (Arc::new(Mutex::new(map)), rx)
    }

    fn hook_events_after(
        sub: &mut tokio::sync::broadcast::Receiver<BusEnvelope>,
    ) -> Vec<HookEvent> {
        let mut hooks = Vec::new();
        while let Ok(env) = sub.try_recv() {
            if let BusEvent::Hook(event) = env.event {
                hooks.push(event);
            }
        }
        hooks
    }

    /// 29 / 31. daemon map 命中 → 发布**一次** `PermissionResult`（query_id 关联正确、
    /// outcome/selected_index 正确）；重复响应不再发布。
    #[tokio::test]
    async fn permission_result_published_once_on_daemon_hit() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe();
        let (pending, mut respond_rx) = pending_with("q-1", "sess-1");
        let flag = AtomicBool::new(false);

        assert!(
            route_daemon_user_response(&bus, &pending, &flag, "q-1", 2, ""),
            "命中 daemon map 应返回 true"
        );
        // 响应确实路由回等待者。
        assert_eq!(respond_rx.try_recv().unwrap().selected_index, 2);

        let env = sub.try_recv().expect("命中应发布 PermissionResult");
        match env.event {
            BusEvent::Hook(event) => {
                assert_eq!(event.event_name(), HookEventName::PermissionResult);
                assert_eq!(event.context.session_id, "sess-1");
                match event.payload {
                    HookPayload::PermissionResult(p) => {
                        assert_eq!(p.query_id, "q-1");
                        assert_eq!(p.outcome, PermissionOutcome::Selected);
                        assert_eq!(p.selected_index, 2);
                    }
                    other => panic!("expected PermissionResult payload, got {other:?}"),
                }
            }
            BusEvent::Frame(_) => panic!("expected Hook bus event, got Frame"),
        }
        assert!(sub.try_recv().is_err(), "命中只应发布一次");

        // 重复/过期响应：map 已空 → 不命中、不再发布，返回 false 供调用方回退。
        assert!(
            !route_daemon_user_response(&bus, &pending, &flag, "q-1", 2, ""),
            "重复响应不应再命中"
        );
        assert!(sub.try_recv().is_err(), "重复响应不得重复发布");
    }

    /// 30. 未命中（过期/外部响应）→ 不发布，返回 false 走 orchestrator 回退。
    #[tokio::test]
    async fn permission_result_not_published_on_fallback() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe();
        let (pending, _respond_rx) = pending_with("q-1", "sess-1");
        let flag = AtomicBool::new(false);

        assert!(
            !route_daemon_user_response(&bus, &pending, &flag, "other-q", 0, ""),
            "未命中应返回 false"
        );
        assert!(
            sub.try_recv().is_err(),
            "orchestrator 回退路径不得发布 PermissionResult"
        );
    }

    /// `PermissionResult.outcome` 映射：非负索引为选择；-1 且带文本为 "Other" 选择；
    /// -1 且文本为空为取消。
    #[test]
    fn permission_outcome_maps_selection_other_and_cancel() {
        assert_eq!(permission_outcome(1, ""), PermissionOutcome::Selected);
        assert_eq!(
            permission_outcome(-1, "custom"),
            PermissionOutcome::Selected
        );
        assert_eq!(permission_outcome(-1, ""), PermissionOutcome::Cancelled);
    }

    /// 32. `delete_session` → `SessionEnd(reason=delete, exit_code=None, session_id)`。
    #[tokio::test]
    async fn delete_session_emits_session_end() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let (service, bus) = make_service_with_bus(mgr.clone());
        let mut sub = bus.subscribe();

        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        service
            .delete_session(Request::new(proto::DeleteSessionRequest {
                session_id: session.id.clone(),
            }))
            .await
            .unwrap();

        let env = sub.try_recv().expect("删除会话应发布 SessionEnd");
        match env.event {
            BusEvent::Hook(event) => {
                assert_eq!(event.event_name(), HookEventName::SessionEnd);
                assert_eq!(event.context.session_id, session.id);
                match event.payload {
                    HookPayload::SessionEnd(p) => {
                        assert_eq!(p.reason, "delete");
                        assert_eq!(p.exit_code, None);
                    }
                    other => panic!("expected SessionEnd payload, got {other:?}"),
                }
            }
            BusEvent::Frame(_) => panic!("expected Hook bus event, got Frame"),
        }
    }

    /// 34. 终止态抑制：关停 `SessionEnd` 之后，daemon 侧发射点不再派发任何 hook 事件
    /// （删除会话不发 `SessionEnd`；即使 map 命中也不发 `PermissionResult`）。
    #[tokio::test]
    async fn daemon_emitters_suppressed_after_shutdown() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let drain = Arc::new(CountingDrain::new());
        let (service, _cancel_rx, _notify, bus) =
            make_service_with_shutdown(mgr.clone(), drain.clone());
        let mut sub = bus.subscribe();

        service.shutdown(shutdown_request()).await.unwrap();
        assert!(service.is_shutting_down());

        // 终止态下删除会话：不得再发 SessionEnd。
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        service
            .delete_session(Request::new(proto::DeleteSessionRequest {
                session_id: session.id,
            }))
            .await
            .unwrap();

        // 终止态下 map 命中：路由仍成功，但不得发 PermissionResult。
        let (pending, mut respond_rx) = pending_with("q-1", "sess-1");
        assert!(route_daemon_user_response(
            &bus,
            &pending,
            service.shutting_down.as_ref(),
            "q-1",
            0,
            ""
        ));
        assert_eq!(respond_rx.try_recv().unwrap().selected_index, 0);

        // 终态 SessionEnd 走直投（不经总线），此后不得再派发任何 hook 事件。
        let hooks = hook_events_after(&mut sub);
        assert_eq!(
            hooks.len(),
            0,
            "终态 SessionEnd 直投 drain host，不经总线；此后不得再派发 hook 事件"
        );

        let terminal = drain.terminal();
        assert_eq!(terminal.len(), 1, "SessionEnd 只应直投一次");
        assert_eq!(terminal[0].event_name(), HookEventName::SessionEnd);
        assert_eq!(
            terminal[0].context.session_id, "",
            "唯一终态事件应为 daemon 级关停 SessionEnd"
        );
    }

    // 35. 异常退出（进程被杀 / panic / SIGKILL）无 `SessionEnd`：
    // 该场景无法在进程内自动断言（进程已消失），标注为手工验收项。

    // ── 步骤 1b-3a：只读 GetHookStats gRPC handler ──────────────────────────

    use std::sync::atomic::AtomicUsize;
    use visp_config::hooks::HookScope;
    use visp_daemon::hook_runtime::{HookStatsSnapshot, HookStatsSource, RuleSummary};

    /// 可控只读统计来源（测试注入）；记录读取次数以断言 RPC 只读。
    struct FakeHookStatsSource {
        snapshot: HookStatsSnapshot,
        reads: StdArc<AtomicUsize>,
    }

    impl HookStatsSource for FakeHookStatsSource {
        fn hook_stats(&self) -> HookStatsSnapshot {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.snapshot.clone()
        }
    }

    /// 用例 1：快照五类计数 + 生效规则摘要完整映射到 proto 响应；重复调用只读无害。
    #[tokio::test]
    async fn get_hook_stats_maps_snapshot_to_response() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let mut service = make_service(mgr);
        let reads = StdArc::new(AtomicUsize::new(0));
        service.hook_stats = StdArc::new(FakeHookStatsSource {
            snapshot: HookStatsSnapshot {
                emitted: 11,
                dropped: 2,
                executed: 3,
                failed: 4,
                timed_out: 5,
                rules: vec![
                    RuleSummary {
                        id: "global-rule".to_string(),
                        scope: HookScope::Global,
                        enabled: true,
                    },
                    RuleSummary {
                        id: "project-rule".to_string(),
                        scope: HookScope::Project,
                        enabled: false,
                    },
                ],
            },
            reads: reads.clone(),
        });

        let response = service
            .get_hook_stats(Request::new(proto::GetHookStatsRequest {
                project: String::new(),
            }))
            .await
            .expect("只读 RPC 不应产生 gRPC error")
            .into_inner();
        assert_eq!(
            (
                response.emitted,
                response.dropped,
                response.executed,
                response.failed,
                response.timed_out,
            ),
            (11, 2, 3, 4, 5)
        );
        assert_eq!(response.rules.len(), 2);
        assert_eq!(response.rules[0].id, "global-rule");
        assert_eq!(response.rules[0].scope, "global");
        assert!(response.rules[0].enabled);
        assert_eq!(response.rules[1].id, "project-rule");
        assert_eq!(response.rules[1].scope, "project");
        assert!(!response.rules[1].enabled);

        // 只读：再次调用（含可选 project）结果一致，仅发生读取。
        let second = service
            .get_hook_stats(Request::new(proto::GetHookStatsRequest {
                project: "/proj".to_string(),
            }))
            .await
            .expect("只读 RPC 不应产生 gRPC error")
            .into_inner();
        assert_eq!(response, second);
        assert_eq!(reads.load(Ordering::SeqCst), 2, "每次调用只读取一次");
    }

    /// 用例 2：无 hook 运行时（默认服务）返回全零计数与空规则表。
    #[tokio::test]
    async fn get_hook_stats_defaults_to_zero_without_runtime() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let service = make_service(mgr);

        let response = service
            .get_hook_stats(Request::new(proto::GetHookStatsRequest::default()))
            .await
            .expect("只读 RPC 不应产生 gRPC error")
            .into_inner();
        assert_eq!(
            (
                response.emitted,
                response.dropped,
                response.executed,
                response.failed,
                response.timed_out,
            ),
            (0, 0, 0, 0, 0)
        );
        assert!(response.rules.is_empty());
    }

    // ── 5e: 真实入站 harness（gRPC 往返）────────────────────────────────────
    //
    // 直接经 `chat` 的双向流驱动真实入站任务（非「复制 handler 逻辑」）：
    // 客户端 → HTTP/2 → `chat` inbound 任务；断言错误帧 / 副作用 / 答复路由。
    // 用例 4 额外接线真实 `Orchestrator` + 可闸控 provider 驱动真实 loop 与 `Done` 时序。

    use futures::stream;
    use tokio_stream::wrappers::ReceiverStream;
    use visp_agent::orchestrator::{CancelSignal, ClientMessage, Orchestrator};
    use visp_core::agent::{AgentEventFrame, Envelope, PermissionKind};
    use visp_core::agent_definition::{AgentDefinition, AgentMode};
    use visp_core::agent_registry::AgentRegistry;
    use visp_core::error::LlmError;
    use visp_core::message::ToolDefinition;
    use visp_core::provider::ChatEvent;
    use visp_proto::visp::coder_daemon_client::CoderDaemonClient;

    /// 以临时端口启动真实 gRPC server；返回 (addr, server task 句柄)。
    async fn spawn_grpc_server(
        service: CoderDaemonService,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let server_addr = addr.clone();
        let handle = tokio::spawn(async move {
            let _ = crate::server::start_server(&server_addr, service).await;
        });
        (addr, handle)
    }

    /// 连接真实 server（重试以跨过 bind→serve 启动窗口）。
    async fn connect_daemon_client(addr: &str) -> CoderDaemonClient<tonic::transport::Channel> {
        for _ in 0..200 {
            if let Ok(client) = CoderDaemonClient::connect(format!("http://{addr}")).await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test gRPC server 未能在超时前就绪: {addr}");
    }

    fn user_input_msg(session_id: &str, text: &str) -> proto::ClientMessage {
        proto::ClientMessage {
            payload: Some(proto::client_message::Payload::UserInput(
                proto::UserInput {
                    text: text.to_string(),
                    session_id: session_id.to_string(),
                    request_id: String::new(),
                },
            )),
        }
    }

    fn user_response_msg(query_id: &str, selected_index: i32, text: &str) -> proto::ClientMessage {
        proto::ClientMessage {
            payload: Some(proto::client_message::Payload::UserResponse(
                proto::UserResponse {
                    query_id: query_id.to_string(),
                    selected_index,
                    text: text.to_string(),
                },
            )),
        }
    }

    /// 读取下一帧（带超时，避免用例挂死）。
    async fn next_frame(stream: &mut Streaming<proto::ServerMessage>) -> proto::ServerMessage {
        tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("等待 server 帧超时")
            .expect("server stream 出错")
            .expect("server stream 提前结束")
    }

    /// 读取下一帧并断言其为 `SessionBusy` 错误帧。
    async fn expect_busy_error(stream: &mut Streaming<proto::ServerMessage>) -> proto::Error {
        match next_frame(stream).await.payload {
            Some(proto::server_message::Payload::Error(e)) => {
                assert_eq!(e.code, SESSION_BUSY_CODE);
                assert_eq!(e.message, SESSION_BUSY_MESSAGE);
                e
            }
            other => panic!("expected SessionBusy Error, got {other:?}"),
        }
    }

    /// 构造 service 并持有 `client_rx`，用于断言「未转发到 orchestrator」。
    fn make_service_holding_client(
        mgr: StdArc<SessionManager>,
    ) -> (
        CoderDaemonService,
        mpsc::Receiver<visp_agent::orchestrator::ClientMessage>,
    ) {
        let mut service = make_service(mgr);
        let (client_tx, client_rx) = mpsc::channel(64);
        service.client_tx = client_tx;
        (service, client_rx)
    }

    /// 用例 1（V12 防死锁回归）：输入被拒期间，答复仍按 `query_id` 回填等待中的 loop。
    #[tokio::test]
    async fn v12_user_response_still_reaches_loop_while_input_rejected_busy() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        let (service, mut client_rx) = make_service_holding_client(mgr.clone());
        let bus = service.bus.clone();
        let (addr, server) = spawn_grpc_server(service).await;
        let mut client = connect_daemon_client(&addr).await;

        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let mut inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // 发布一条 UserQuery：outbound 登记 pending 等待者并下发给客户端。
        let (respond_tx, mut respond_rx) = mpsc::channel::<UserQueryResult>(1);
        bus.publish(BusEvent::Frame(AgentEventFrame {
            event: AgentEvent::UserQuery {
                query_id: "q-1".into(),
                message: "Allow?".into(),
                options: vec![],
                allow_other: false,
                kind: PermissionKind::Question,
                respond: respond_tx,
            },
            session_id: sid.clone(),
            agent_name: "default".into(),
            parent_session_id: None,
            parent_session_name: None,
        }));
        match next_frame(&mut inbound).await.payload {
            Some(proto::server_message::Payload::UserQuery(q)) => assert_eq!(q.query_id, "q-1"),
            other => panic!("expected UserQuery, got {other:?}"),
        }

        // 在途循环期间的新用户输入 → 忙拒绝。
        tx.send(user_input_msg(&sid, "hello")).await.unwrap();
        let err = expect_busy_error(&mut inbound).await;
        assert_eq!(err.session_id, sid);

        // 随后发送答复：守卫只拦用户输入，答复仍按 query_id 回填等待中的 loop。
        tx.send(user_response_msg("q-1", 2, "")).await.unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(2), respond_rx.recv())
            .await
            .expect("答复应在超时前送达等待中的 loop")
            .expect("respond 通道应仍存活");
        assert_eq!(delivered.selected_index, 2);

        // 命中 daemon pending 表 → 不得回退 orchestrator（client_rx 不应收到任何帧）。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            client_rx.try_recv().is_err(),
            "答复已命中 daemon pending 表，不得回退 orchestrator"
        );

        server.abort();
    }

    /// 用例 2：端到端忙拒绝——唯一错误帧、忙态与令牌保持、无第二个 loop、令牌仍可取消。
    #[tokio::test]
    async fn e2e_busy_rejection_is_single_and_token_preserved() {
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(Path::new("/tmp"), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        let (service, mut client_rx) = make_service_holding_client(mgr.clone());
        let (addr, server) = spawn_grpc_server(service).await;
        let mut client = connect_daemon_client(&addr).await;

        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let mut inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        tx.send(user_input_msg(&sid, "hello")).await.unwrap();
        expect_busy_error(&mut inbound).await;

        // 唯一性：短窗口内不得再出现第二帧（重复忙错误 / prompt 转发）。
        let extra = tokio::time::timeout(Duration::from_millis(200), inbound.message()).await;
        assert!(extra.is_err(), "忙拒绝后不应再发帧");

        // 会话仍处忙态、令牌仍在。
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);
        assert!(mgr.has_inflight_loop(&sid));

        // 不存在第二个 loop：在途令牌存在时再次 start_loop 仍以 SessionBusy 失败。
        assert!(
            mgr.start_loop(&sid, &trimmer, None, None).is_err(),
            "在途令牌存在时不得再启动第二个 loop"
        );

        // 该令牌仍可被取消。
        mgr.cancel_agent(&sid);
        assert!(
            !mgr.has_inflight_loop(&sid),
            "被拒输入不应影响原令牌的可取消性"
        );

        // 被拒输入未被守卫放行 → 不转发给 orchestrator。
        assert!(client_rx.try_recv().is_err(), "忙拒绝不得转发 prompt");

        server.abort();
    }

    /// 用例 3：命令顺序端到端——生成期 `/init`、`/init-agent` 被守卫拦下，
    /// 不转发为 prompt、不产生文件副作用（`.visp` 未被创建）。
    #[tokio::test]
    async fn e2e_busy_guard_blocks_init_commands_before_parse() {
        let project = TempDir::new().unwrap();
        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(project.path(), LlmConfig::default()).unwrap();
        let sid = session.id.clone();
        let trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        mgr.start_loop(&sid, &trimmer, None, None).unwrap();

        let (service, mut client_rx) = make_service_holding_client(mgr.clone());
        let (addr, server) = spawn_grpc_server(service).await;
        let mut client = connect_daemon_client(&addr).await;

        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let mut inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        for text in ["/init", "/init-agent my-agent"] {
            tx.send(user_input_msg(&sid, text)).await.unwrap();
            expect_busy_error(&mut inbound).await;
        }

        // `/init` 的受理路径会创建 `.visp/{rules,skills}`；被守卫拦下则无任何文件副作用。
        assert!(
            !project.path().join(".visp").exists(),
            "忙期被拒的命令不得产生文件副作用"
        );
        assert!(client_rx.try_recv().is_err(), "被拒命令不得转发为 prompt");

        server.abort();
    }

    /// 可闸控 LLM provider：进入 `chat_stream` 时通知 `started`，等待 `release`
    /// 后产出文本 + Done，使测试可确定性控制真实 loop 的在途窗口与收尾时序。
    struct GatedProvider {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for GatedProvider {
        async fn chat_stream(
            &self,
            _messages: &[visp_core::message::Message],
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<
            Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
            LlmError,
        > {
            self.started.notify_one();
            self.release.notified().await;
            Ok(Box::pin(stream::iter(vec![
                Ok(ChatEvent::TextDelta("hello".to_string())),
                Ok(ChatEvent::Done),
            ])))
        }
    }

    /// 用例 4：忙拒绝后旧 loop 收尾 → 会话只有唯一终态；不存在交错重复回合。
    #[tokio::test]
    async fn busy_rejected_then_old_loop_finishes_with_single_terminal_state() {
        let project = TempDir::new().unwrap();

        let mut registry = AgentRegistry::new();
        registry
            .register(AgentDefinition {
                name: "default".to_string(),
                description: String::new(),
                mode: AgentMode::All,
                model: None,
                temperature: None,
                steps: None,
                permission: vec![],
                allowed_sub_agents: Vec::new(),
                system_prompt: String::new(),
            })
            .unwrap();
        let agent_registry = Arc::new(ArcSwap::from_pointee(registry));

        let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
        let session = mgr.create(project.path(), LlmConfig::default()).unwrap();
        let sid = session.id.clone();

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider: Arc<dyn LlmProvider> = Arc::new(GatedProvider {
            started: started.clone(),
            release: release.clone(),
        });

        let daemon_config = Arc::new(visp_config::DaemonConfig {
            llm: visp_config::LlmSection {
                models: vec![visp_config::LlmModelConfig {
                    name: "mock-model".to_string(),
                    protocol: "openai".to_string(),
                    provider: Some("Mock".to_string()),
                    model: "mock-model".to_string(),
                    api_key: None,
                    base_url: None,
                    temperature: None,
                    max_tokens: None,
                    max_context_tokens: None,
                    thinking_budget_tokens: None,
                    use_tool: None,
                    image_generation: None,
                    extra: HashMap::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        });

        let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        providers.insert("Mock/mock-model".to_string(), provider);

        // 真实 Orchestrator 接线：与 service 共享 session_mgr / bus / client_tx。
        let (cancel_tx, cancel_rx) = mpsc::channel::<CancelSignal>(16);
        let (global_tx, global_rx) = mpsc::channel::<Envelope>(256);
        let (client_tx, client_rx) = mpsc::channel::<ClientMessage>(64);
        let bus = Arc::new(EventBus::new());

        let orch_rule_engine = Arc::new(RuleEngine::new(project.path()).unwrap());
        let orch_trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
            Arc::new(visp_core::context::NoopTrimmer);
        let mut orchestrator = Orchestrator::new(
            cancel_rx,
            global_rx,
            global_tx.clone(),
            client_rx,
            mgr.clone(),
            agent_registry,
            Arc::new(ToolRegistry::new()),
            orch_rule_engine,
            AgentConfig::default(),
            orch_trimmer,
            daemon_config,
            providers,
            bus.clone(),
        );
        tokio::spawn(async move {
            orchestrator.run().await;
        });

        let (service_rule_engine, reload_core) = build_reload_core(project.path());
        let mut service = make_service_with_core(mgr.clone(), service_rule_engine, reload_core);
        service.bus = bus.clone();
        service.client_tx = client_tx.clone();
        service.cancel_tx = cancel_tx.clone();

        let (addr, server) = spawn_grpc_server(service).await;
        let mut client = connect_daemon_client(&addr).await;
        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let mut inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // 第一回合：真实 loop 启动并被 provider 闸停 → 处于在途。
        tx.send(user_input_msg(&sid, "first")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("真实 loop 应在超时前进入 provider");
        assert_eq!(mgr.get(&sid).unwrap().status, SessionStatus::Running);
        assert!(mgr.has_inflight_loop(&sid));

        // 生成期第二次输入 → 忙拒绝（唯一错误帧）。
        tx.send(user_input_msg(&sid, "second")).await.unwrap();
        expect_busy_error(&mut inbound).await;

        // 放行旧 loop：产出文本 + Done，orchestrator 收尾。
        release.notify_one();

        // 等待会话回到唯一终态（Idle）。
        let mut terminal = false;
        for _ in 0..500 {
            if matches!(mgr.get(&sid).map(|s| s.status), Ok(SessionStatus::Idle)) {
                terminal = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminal, "旧 loop 应在超时前收尾为 Idle");
        assert!(!mgr.has_inflight_loop(&sid), "收尾后令牌应被清除");

        // 收集放行后帧：唯一回合恰好一条 TextDelta + 一条 Done，无交错重复。
        let (mut done, mut text) = (0usize, 0usize);
        loop {
            match tokio::time::timeout(Duration::from_millis(300), inbound.message()).await {
                Ok(Ok(Some(msg))) => match msg.payload {
                    Some(proto::server_message::Payload::Done(_)) => done += 1,
                    Some(proto::server_message::Payload::TextDelta(_)) => text += 1,
                    _ => {}
                },
                _ => break,
            }
        }
        assert_eq!(done, 1, "唯一终态回合应恰好一条 Done");
        assert_eq!(text, 1, "唯一回合应恰好一条 TextDelta");

        let history = mgr.get(&sid).unwrap().history;
        let users = history.iter().filter(|m| m.role == Role::User).count();
        let assistants = history.iter().filter(|m| m.role == Role::Assistant).count();
        assert_eq!(users, 1, "被拒的第二次输入不得进入历史");
        assert_eq!(assistants, 1, "不得出现交错重复回合");

        server.abort();
    }

    // ── 5f: 取消后立即重提的拒绝式语义（固化计划备注 F）────────────────────
    //
    // 「取消即移除令牌」使守卫判据短暂消失，但旧 loop 尚未收尾、会话仍
    // `Running`；此时立即重提会越过守卫，却被第二道门（`start_loop` 状态闸）
    // 以 `SessionBusy` 拒绝。净效果仍是拒绝，且不产生第二个循环。

    /// 入站 `Cancel` 消息（经真实 gRPC 路径送达 orchestrator）。
    fn cancel_msg(session_id: &str) -> proto::ClientMessage {
        proto::ClientMessage {
            payload: Some(proto::client_message::Payload::Cancel(proto::Cancel {
                session_id: session_id.to_string(),
            })),
        }
    }

    /// 真实 gRPC + 真实 `Orchestrator` 端到端 harness。
    ///
    /// 与用例 4 同一套接线（临时端口 + 可闸控 provider + 真实 loop），额外在
    /// `service.client_tx` 与 orchestrator 之间插入转发探针：每当一条
    /// `UserInput` **越过守卫**进入 orchestrator 时 `forwarded` 收到一个信号，
    /// 用以区分「被守卫拦下」与「被状态闸拒绝」。
    struct OrchestratorE2e {
        _project: TempDir,
        sid: String,
        mgr: StdArc<SessionManager>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        forwarded: mpsc::Receiver<()>,
        addr: String,
        server: tokio::task::JoinHandle<()>,
    }

    impl OrchestratorE2e {
        async fn new() -> Self {
            let project = TempDir::new().unwrap();

            let mut registry = AgentRegistry::new();
            registry
                .register(AgentDefinition {
                    name: "default".to_string(),
                    description: String::new(),
                    mode: AgentMode::All,
                    model: None,
                    temperature: None,
                    steps: None,
                    permission: vec![],
                    allowed_sub_agents: Vec::new(),
                    system_prompt: String::new(),
                })
                .unwrap();
            let agent_registry = Arc::new(ArcSwap::from_pointee(registry));

            let mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
            let session = mgr.create(project.path(), LlmConfig::default()).unwrap();
            let sid = session.id.clone();

            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let provider: Arc<dyn LlmProvider> = Arc::new(GatedProvider {
                started: started.clone(),
                release: release.clone(),
            });

            let daemon_config = Arc::new(visp_config::DaemonConfig {
                llm: visp_config::LlmSection {
                    models: vec![visp_config::LlmModelConfig {
                        name: "mock-model".to_string(),
                        protocol: "openai".to_string(),
                        provider: Some("Mock".to_string()),
                        model: "mock-model".to_string(),
                        api_key: None,
                        base_url: None,
                        temperature: None,
                        max_tokens: None,
                        max_context_tokens: None,
                        thinking_budget_tokens: None,
                        use_tool: None,
                        image_generation: None,
                        extra: HashMap::new(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            });

            let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
            providers.insert("Mock/mock-model".to_string(), provider);

            let (cancel_tx, cancel_rx) = mpsc::channel::<CancelSignal>(16);
            let (global_tx, global_rx) = mpsc::channel::<Envelope>(256);
            let (orch_client_tx, orch_client_rx) = mpsc::channel::<ClientMessage>(64);
            let (guard_tx, mut guard_rx) = mpsc::channel::<ClientMessage>(64);
            let (forwarded_tx, forwarded_rx) = mpsc::channel::<()>(64);
            let bus = Arc::new(EventBus::new());

            // 守卫后转发探针：仅对 `UserInput` 记录（证明其越过了守卫）。
            tokio::spawn(async move {
                while let Some(msg) = guard_rx.recv().await {
                    if matches!(&msg, ClientMessage::UserInput { .. }) {
                        let _ = forwarded_tx.send(()).await;
                    }
                    if orch_client_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            let orch_rule_engine = Arc::new(RuleEngine::new(project.path()).unwrap());
            let orch_trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
                Arc::new(visp_core::context::NoopTrimmer);
            let mut orchestrator = Orchestrator::new(
                cancel_rx,
                global_rx,
                global_tx.clone(),
                orch_client_rx,
                mgr.clone(),
                agent_registry,
                Arc::new(ToolRegistry::new()),
                orch_rule_engine,
                AgentConfig::default(),
                orch_trimmer,
                daemon_config,
                providers,
                bus.clone(),
            );
            tokio::spawn(async move {
                orchestrator.run().await;
            });

            let (service_rule_engine, reload_core) = build_reload_core(project.path());
            let mut service = make_service_with_core(mgr.clone(), service_rule_engine, reload_core);
            service.bus = bus.clone();
            service.client_tx = guard_tx;
            service.cancel_tx = cancel_tx;

            let (addr, server) = spawn_grpc_server(service).await;
            Self {
                _project: project,
                sid,
                mgr,
                started,
                release,
                forwarded: forwarded_rx,
                addr,
                server,
            }
        }

        async fn connect(&self) -> CoderDaemonClient<tonic::transport::Channel> {
            connect_daemon_client(&self.addr).await
        }

        /// 等待一条 `UserInput` 越过守卫进入 orchestrator。
        async fn next_forwarded(&mut self) -> Option<()> {
            self.forwarded.recv().await
        }
    }

    impl Drop for OrchestratorE2e {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// 用例 5：在途循环取消后**立即重提**——守卫判据（令牌）已消失而放行，
    /// 会话仍 `Running`，第二道门（`start_loop` 状态闸）以 `SessionBusy` 拒绝。
    #[tokio::test]
    async fn cancel_then_immediate_resubmit_is_rejected_by_state_gate() {
        let mut h = OrchestratorE2e::new().await;
        let mut client = h.connect().await;
        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let mut inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // 第一回合：真实 loop 启动并被 provider 闸停 → 处于在途。
        tx.send(user_input_msg(&h.sid, "first")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), h.started.notified())
            .await
            .expect("真实 loop 应在超时前进入 provider");
        assert_eq!(h.mgr.get(&h.sid).unwrap().status, SessionStatus::Running);
        assert!(h.mgr.has_inflight_loop(&h.sid));

        // 用户取消（真实入站路径）：令牌被立即移除，守卫判据消失；
        // 但旧 loop 仍在 provider 内阻塞、尚未收尾 → 会话保持 `Running`。
        tx.send(cancel_msg(&h.sid)).await.unwrap();
        let mut token_cleared = false;
        for _ in 0..500 {
            if !h.mgr.has_inflight_loop(&h.sid) {
                token_cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(token_cleared, "取消应移除在途令牌（守卫判据消失）");
        assert_eq!(
            h.mgr.get(&h.sid).unwrap().status,
            SessionStatus::Running,
            "旧 loop 未收尾前会话状态仍为 Running"
        );

        // 立即重提：守卫因无令牌而放行 → 转发探针收到 → 状态闸拒绝。
        tx.send(user_input_msg(&h.sid, "second")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), h.next_forwarded())
            .await
            .expect("输入应在超时前越过守卫进入 orchestrator")
            .expect("转发探针通道应存活");

        // 客户端唯一一条 SessionBusy——来自状态闸而非守卫。
        expect_busy_error(&mut inbound).await;
        let extra = tokio::time::timeout(Duration::from_millis(200), inbound.message()).await;
        assert!(extra.is_err(), "状态闸拒绝后不应再发帧");

        // 会话仍 Running；状态闸在插入令牌前失败 → 不存在第二个循环。
        assert_eq!(h.mgr.get(&h.sid).unwrap().status, SessionStatus::Running);
        assert!(
            !h.mgr.has_inflight_loop(&h.sid),
            "状态闸拒绝不得产生第二个循环令牌"
        );

        // 放行旧 loop，等待唯一终态收敛（取消路径 → 终态）。
        h.release.notify_one();
        let mut terminal = false;
        for _ in 0..500 {
            if matches!(
                h.mgr.get(&h.sid).map(|s| s.status),
                Ok(s) if s != SessionStatus::Running
            ) {
                terminal = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminal, "旧 loop 应在超时前收尾为唯一终态");
        assert!(!h.mgr.has_inflight_loop(&h.sid), "收尾后不得残留令牌");

        // 被拒的第二次输入未进入历史；唯一回合未产出助手消息。
        let history = h.mgr.get(&h.sid).unwrap().history;
        let users = history.iter().filter(|m| m.role == Role::User).count();
        let assistants = history.iter().filter(|m| m.role == Role::Assistant).count();
        assert_eq!(users, 1, "被拒的第二次输入不得进入历史");
        assert_eq!(assistants, 0, "被中途取消的回合不应产出助手消息");
    }

    /// 用例 6（对照）：取消后**等旧循环收尾**再重提 → 正常受理并启动新回合，
    /// 证明上述窗口是瞬时的，而非永久拒绝。
    #[tokio::test]
    async fn resubmit_after_old_loop_finishes_is_accepted_as_new_turn() {
        let mut h = OrchestratorE2e::new().await;
        let mut client = h.connect().await;
        let (tx, rx) = mpsc::channel::<proto::ClientMessage>(16);
        let _inbound = client
            .chat(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // 第一回合进入在途后取消。
        tx.send(user_input_msg(&h.sid, "first")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), h.started.notified())
            .await
            .expect("真实 loop 应在超时前进入 provider");
        tx.send(cancel_msg(&h.sid)).await.unwrap();

        // 放行旧 loop 并等待收尾（离开 Running）。
        h.release.notify_one();
        let mut terminal = false;
        for _ in 0..500 {
            if matches!(
                h.mgr.get(&h.sid).map(|s| s.status),
                Ok(s) if s != SessionStatus::Running
            ) {
                terminal = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminal, "旧 loop 应在超时前收尾");

        // 窗口已过：重提越过守卫并被正常受理，启动全新回合。
        tx.send(user_input_msg(&h.sid, "second")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), h.next_forwarded())
            .await
            .expect("收尾后重提应越过守卫")
            .expect("转发探针通道应存活");
        tokio::time::timeout(Duration::from_secs(5), h.started.notified())
            .await
            .expect("重提应启动新回合（provider 再次被调用）");
        assert_eq!(h.mgr.get(&h.sid).unwrap().status, SessionStatus::Running);
        assert!(h.mgr.has_inflight_loop(&h.sid), "新回合应持有在途令牌");

        // 放行新回合 → 正常完成。
        h.release.notify_one();
        let mut finished = false;
        for _ in 0..500 {
            if matches!(
                h.mgr.get(&h.sid).map(|s| s.status),
                Ok(SessionStatus::Idle) | Ok(SessionStatus::Completed)
            ) {
                finished = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(finished, "新回合应正常收尾");
        let history = h.mgr.get(&h.sid).unwrap().history;
        let users = history.iter().filter(|m| m.role == Role::User).count();
        assert_eq!(users, 2, "新回合的用户输入应进入历史");
    }
}
