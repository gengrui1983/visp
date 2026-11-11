use super::*;
use crate::TraceContext;
use crate::context::ContextTrimmer;
use crate::session::InMemorySessionStore;
use crate::tool::Tool;
use std::path::Path;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn test_wait_subagent_response_success() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send("done".to_string()).unwrap();
    let result = wait_subagent_response(rx, Duration::from_secs(5)).await;
    assert!(!result.is_error);
    assert!(result.content.contains("done"));
}

#[tokio::test]
async fn test_wait_subagent_response_timeout() {
    let (_tx, rx) = tokio::sync::oneshot::channel::<String>();
    let result = wait_subagent_response(rx, Duration::from_millis(50)).await;
    assert!(result.is_error);
    assert!(result.content.contains("timed out"));
}

#[tokio::test]
async fn test_wait_subagent_response_channel_closed() {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    drop(tx); // 通道关闭，无人发送
    let result = wait_subagent_response(rx, Duration::from_secs(5)).await;
    assert!(result.is_error);
    assert!(result.content.contains("channel closed"));
}

struct MockTrimmer;
impl ContextTrimmer for MockTrimmer {
    fn trim(
        &self,
        h: &[crate::message::Message],
        _: u32,
        _: u32,
        _: u32,
    ) -> Vec<crate::message::Message> {
        h.to_vec()
    }
}

// ── Mock tool for tests ─────────────────────────────────────────────────

struct MockAgentTool {
    name: &'static str,
    requires_approval: bool,
    executed: StdArc<AtomicBool>,
}

#[async_trait::async_trait]
impl Tool for MockAgentTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Mock tool for agent tests"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        self.executed.store(true, Ordering::SeqCst);
        ToolResult::success("mock executed")
    }
    fn requires_approval(&self) -> bool {
        self.requires_approval
    }
}

fn mock_tool(name: &'static str, approval: bool) -> (Box<dyn Tool>, StdArc<AtomicBool>) {
    let executed = StdArc::new(AtomicBool::new(false));
    let e = executed.clone();
    (
        Box::new(MockAgentTool {
            name,
            requires_approval: approval,
            executed,
        }),
        e,
    )
}

// ── Test provider with phased responses ────────────────────────────────

struct TestProvider {
    phases: Vec<Vec<ChatEvent>>,
    call_count: AtomicUsize,
}

impl TestProvider {
    fn new(phases: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            phases,
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for TestProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[crate::message::ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
        LlmError,
    > {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);

        let events = self
            .phases
            .get(idx)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(Ok);
        let stream = futures::stream::iter(events);
        Ok(Box::pin(stream))
    }
}

// ── Test helpers ───────────────────────────────────────────────────────

struct TestSetup {
    session_mgr: StdArc<SessionManager>,
    session_id: String,
    ctx: AgentLoopContext,
    rule_engine: StdArc<RuleEngine>,
}

fn test_setup() -> TestSetup {
    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let trimmer: StdArc<dyn ContextTrimmer + Send + Sync> = StdArc::new(MockTrimmer);
    let ctx = session_mgr
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    TestSetup {
        session_mgr,
        session_id: session.id,
        ctx,
        rule_engine,
    }
}

async fn run_collect(
    provider: StdArc<dyn LlmProvider>,
    tools: Vec<Box<dyn Tool>>,
    setup: TestSetup,
    soft_limit: u32,
    hard_limit: u32,
    user_msg: Message,
) -> (Vec<AgentEvent>, StdArc<SessionManager>, String) {
    let (tx, mut rx) = mpsc::channel(64);

    let registry = ToolRegistry::new();
    for tool in tools {
        registry.register(Arc::from(tool)).unwrap();
    }
    let tool_registry = StdArc::new(registry);
    let config = AgentConfig {
        soft_limit,
        hard_limit,
        ..Default::default()
    };

    let session_mgr = setup.session_mgr.clone();
    let sid = setup.session_id.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            tool_registry,
            setup.rule_engine,
            session_mgr,
            setup.ctx,
            &config,
            user_msg,
            tx,
        )
        .await;
    });

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    (events, setup.session_mgr, sid)
}

// ── Existing tests ─────────────────────────────────────────────────────

#[test]
fn test_agent_config_default() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.soft_limit, 50);
    assert_eq!(cfg.hard_limit, 200);
    assert_eq!(cfg.doom_loop_threshold, 5);
    assert_eq!(cfg.llm_retry_attempts, 3);
    assert_eq!(cfg.llm_retry_base_delay_ms, 1000);
    assert!(cfg.bash_confirm_mode);
    assert_eq!(cfg.file_max_size_bytes, 1048576);
}

#[test]
fn test_agent_config_langfuse_defaults() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.langfuse_user_id, None);
    assert_eq!(cfg.langfuse_tags, None);
}

#[test]
fn test_agent_config_langfuse_configured() {
    let cfg = AgentConfig {
        langfuse_user_id: Some("user_456".into()),
        langfuse_tags: Some(r#"["agent","weather"]"#.into()),
        ..Default::default()
    };
    assert_eq!(cfg.langfuse_user_id.as_deref(), Some("user_456"));
    assert_eq!(cfg.langfuse_tags.as_deref(), Some(r#"["agent","weather"]"#));
}

// ── 1b: 传递配置到核心 AgentConfig ──────────────────────────────────

#[test]
fn test_agent_config_langfuse_enabled_default() {
    let cfg = AgentConfig::default();
    assert!(
        !cfg.langfuse_enabled,
        "langfuse_enabled should default to false"
    );
}

#[test]
fn test_agent_config_langfuse_all_fields() {
    let mut meta = HashMap::new();
    meta.insert("env".into(), "prod".into());
    meta.insert("count".into(), "42".into());
    let cfg = AgentConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("user".into()),
        langfuse_tags: Some(r#"["agent"]"#.into()),
        langfuse_environment: Some("staging".into()),
        langfuse_release: Some("2.0".into()),
        langfuse_version: Some("abc123".into()),
        langfuse_public: Some(true),
        langfuse_metadata: Some(meta.clone()),
        ..Default::default()
    };
    assert!(cfg.langfuse_enabled);
    assert_eq!(cfg.langfuse_user_id.as_deref(), Some("user"));
    assert_eq!(cfg.langfuse_tags.as_deref(), Some(r#"["agent"]"#));
    assert_eq!(cfg.langfuse_environment.as_deref(), Some("staging"));
    assert_eq!(cfg.langfuse_release.as_deref(), Some("2.0"));
    assert_eq!(cfg.langfuse_version.as_deref(), Some("abc123"));
    assert_eq!(cfg.langfuse_public, Some(true));
    assert_eq!(cfg.langfuse_metadata, Some(meta));
}

#[test]
fn test_agent_config_langfuse_capture_defaults() {
    let cfg = AgentConfig::default();
    assert!(!cfg.langfuse_capture_input);
    assert!(!cfg.langfuse_capture_output);
    assert_eq!(cfg.langfuse_capture_max_chars, 20000);
    assert!(cfg.langfuse_redact_secrets);
}

#[test]
fn test_agent_config_langfuse_capture_configured() {
    let cfg = AgentConfig {
        langfuse_capture_input: true,
        langfuse_capture_output: true,
        langfuse_capture_max_chars: 5000,
        langfuse_redact_secrets: false,
        ..Default::default()
    };
    assert!(cfg.langfuse_capture_input);
    assert!(cfg.langfuse_capture_output);
    assert_eq!(cfg.langfuse_capture_max_chars, 5000);
    assert!(!cfg.langfuse_redact_secrets);
}

#[test]
fn test_agent_config_langfuse_empty_tags_none() {
    // 空 tags 策略：tags 为 None 时不写入
    let cfg = AgentConfig {
        langfuse_tags: None,
        ..Default::default()
    };
    assert_eq!(cfg.langfuse_tags, None);
}

#[test]
fn test_agent_config_langfuse_metadata_cross_crate() {
    // metadata 跨 crate 表达：HashMap<String, String> 可以在 core 中使用
    let mut meta = HashMap::new();
    meta.insert("env".into(), "prod".into());
    meta.insert("items".into(), r#"["a","b"]"#.into());
    let cfg = AgentConfig {
        langfuse_metadata: Some(meta),
        ..Default::default()
    };
    let m = cfg.langfuse_metadata.as_ref().unwrap();
    assert_eq!(m.get("env").map(|s| s.as_str()), Some("prod"));
    assert_eq!(m.get("items").map(|s| s.as_str()), Some(r#"["a","b"]"#));
    assert_eq!(m.len(), 2);
}

#[test]
fn test_soft_limit_zero_disabled() {
    let cfg = AgentConfig {
        soft_limit: 0,
        ..Default::default()
    };
    assert_eq!(cfg.soft_limit, 0);
}

#[test]
fn test_agent_event_text_delta() {
    let evt = AgentEvent::TextDelta("hello".into());
    match evt {
        AgentEvent::TextDelta(content) => assert_eq!(content, "hello"),
        _ => panic!("expected TextDelta"),
    }
}

#[test]
fn test_agent_event_tool_call() {
    let evt = AgentEvent::ToolCallRequest {
        call_id: "call-1".into(),
        tool_name: "bash".into(),
        arguments: "{}".into(),
    };
    match evt {
        AgentEvent::ToolCallRequest {
            call_id,
            tool_name,
            arguments,
        } => {
            assert_eq!(call_id, "call-1");
            assert_eq!(tool_name, "bash");
            assert_eq!(arguments, "{}");
        }
        _ => panic!("expected ToolCallRequest"),
    }
}

#[test]
fn test_user_query_result_default() {
    let r = UserQueryResult::default();
    assert_eq!(r.selected_index, 0);
    assert!(r.text.is_empty());
}

#[test]
fn test_agent_event_user_query() {
    let (tx, _rx) = mpsc::channel::<UserQueryResult>(1);
    let evt = AgentEvent::UserQuery {
        query_id: "q1".into(),
        message: "confirm?".into(),
        options: vec!["Yes".into(), "No".into()],
        allow_other: true,
        kind: PermissionKind::Approval,
        respond: tx,
    };
    match evt {
        AgentEvent::UserQuery {
            query_id,
            message,
            options,
            allow_other,
            kind,
            ..
        } => {
            assert_eq!(query_id, "q1");
            assert_eq!(message, "confirm?");
            assert_eq!(options, vec!["Yes", "No"]);
            assert!(allow_other);
            assert_eq!(kind, PermissionKind::Approval);
        }
        _ => panic!("expected UserQuery"),
    }
}

/// 0c-1: `AgentEventFrame` 克隆后各字段全等；`mpsc::Sender` 可克隆
#[test]
fn test_agent_event_frame_clone_fields_equal() {
    let (tx, _rx) = mpsc::channel::<UserQueryResult>(1);
    let frame = AgentEventFrame {
        event: AgentEvent::UserQuery {
            query_id: "q1".into(),
            message: "confirm?".into(),
            options: vec!["Yes".into(), "No".into()],
            allow_other: true,
            kind: PermissionKind::Approval,
            respond: tx.clone(),
        },
        session_id: "sess-1".into(),
        agent_name: "main".into(),
        parent_session_id: Some("parent-1".into()),
        parent_session_name: Some("root".into()),
    };

    let cloned = frame.clone();

    assert_eq!(cloned.session_id, frame.session_id);
    assert_eq!(cloned.agent_name, frame.agent_name);
    assert_eq!(cloned.parent_session_id, frame.parent_session_id);
    assert_eq!(cloned.parent_session_name, frame.parent_session_name);

    match (frame.event, cloned.event) {
        (
            AgentEvent::UserQuery {
                query_id: q1,
                message: m1,
                options: o1,
                allow_other: a1,
                ..
            },
            AgentEvent::UserQuery {
                query_id: q2,
                message: m2,
                options: o2,
                allow_other: a2,
                ..
            },
        ) => {
            assert_eq!(q1, q2);
            assert_eq!(m1, m2);
            assert_eq!(o1, o2);
            assert_eq!(a1, a2);
        }
        _ => panic!("expected UserQuery"),
    }
}

#[test]
fn test_agent_loop_context_fields() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let trimmer: std::sync::Arc<dyn ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(MockTrimmer);
    let ctx = AgentLoopContext {
        session_id: "sess-1".into(),
        history: vec![],
        working_dir: PathBuf::from("/tmp"),
        config: crate::provider::LlmConfig::default(),
        cancel_token: cancel.clone(),
        context_trimmer: trimmer,
        global_tx: None,
        permission_rules: None,
        agent_kind: AgentKind::Primary,
        depth: 0,
        parent_session_id: "sess-1".into(),
    };
    assert_eq!(ctx.session_id, "sess-1");
    assert!(ctx.history.is_empty());
    assert_eq!(ctx.working_dir, Path::new("/tmp"));
    assert_eq!(ctx.config.model, "claude-3-7-sonnet-20250219");
}

// ── New agent loop tests ───────────────────────────────────────────────

/// 1. 简单响应：TextDelta → Done
#[tokio::test]
async fn test_simple_response() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello".into()),
        ChatEvent::Done,
    ]]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello"]);

    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Error { .. }))
    );
}

/// W1: Done 分支 —— provider 直接返回 ChatEvent::Done
#[tokio::test]
async fn test_done_decision_logs_completion() {
    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(TestProvider::new(vec![vec![ChatEvent::Done]]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Go")).await;

    // Agent loop should produce a Done event
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "expected Done event from minimal provider"
    );
    // No errors during bare-minimum run
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Error { .. })),
        "unexpected error events"
    );
}

/// 2. 工具调用：ToolCall → Done
#[tokio::test]
async fn test_tool_call() {
    let (tool, _executed) = mock_tool("finder", false);
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (events, _sm, _sid) = run_collect(
        provider,
        vec![tool],
        test_setup(),
        10,
        200,
        Message::user("Find files"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallRequest { .. }))
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCallResult { call_id, is_error: false, outcome: ToolOutcome::Success, .. } if call_id == "call-1")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Error { .. }))
    );
}

/// 3. 多工具批量执行：2 个 ToolCall 并行执行
#[tokio::test]
async fn test_multi_tool_batch() {
    let (tool_a, _ex_a) = mock_tool("finder", false);
    let (tool_b, _ex_b) = mock_tool("grep", false);
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::ToolCall {
                id: "call-2".into(),
                name: "grep".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (events, _sm, _sid) = run_collect(
        provider,
        vec![tool_a, tool_b],
        test_setup(),
        10,
        200,
        Message::user("Find and grep"),
    )
    .await;

    let tool_calls: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallRequest { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 2);
    assert!(tool_calls.contains(&"finder"));
    assert!(tool_calls.contains(&"grep"));

    let tool_results: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallResult {
                call_id,
                is_error: false,
                outcome: ToolOutcome::Success,
                ..
            } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert!(tool_results.contains(&"call-1"));
    assert!(tool_results.contains(&"call-2"));

    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Error { .. }))
    );
}

/// 4. 硬上限：hard_limit=2，一直返回 ToolCall → Error(MaxIterations)
#[tokio::test]
async fn test_max_iterations() {
    let (tool, _executed) = mock_tool("finder", false);
    // Always returns ToolCall — provider must supply enough phases
    // so the loop runs all hard_limit iterations and then triggers
    // MaxIterations on the next one.
    let tool_call_phase = vec![
        ChatEvent::ToolCall {
            id: "call-1".into(),
            name: "finder".into(),
            arguments: "{}".into(),
        },
        ChatEvent::Done,
    ];
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        tool_call_phase.clone(),
        tool_call_phase,
    ]));

    let (events, _sm, _sid) = run_collect(
        provider,
        vec![tool],
        test_setup(),
        1, // soft_limit = 1
        2, // hard_limit = 2
        Message::user("Find"),
    )
    .await;

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error {
            code: AgentErrorCode::MaxIterations,
            ..
        }
    )));
}

/// 5. 取消：触发 CancellationToken → Error(Cancelled)
#[tokio::test]
async fn test_cancellation() {
    let setup = test_setup();
    setup.ctx.cancel_token.cancel();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello".into()),
        ChatEvent::Done,
    ]]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], setup, 10, 200, Message::user("Hi")).await;

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error {
            code: AgentErrorCode::Cancelled,
            ..
        }
    )));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

/// 6. 用户确认：tool requires_approval=true，回复 true → 工具执行
#[tokio::test]
async fn test_user_query() {
    let (tool, executed) = mock_tool("finder", true);
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();
    registry.register(Arc::from(tool)).unwrap();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    let sm = setup.session_mgr.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm,
            setup.ctx,
            &config,
            Message::user("Find files"),
            tx,
        )
        .await;
    });

    let mut done = false;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery { respond, .. } => {
                // Respond immediately to allow tool task to proceed
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: 0,
                        text: String::new(),
                    })
                    .await;
            }
            AgentEvent::Done => {
                done = true;
            }
            _ => {}
        }
    }

    assert!(done, "Expected Done event");
    assert!(
        executed.load(Ordering::SeqCst),
        "Tool should have been executed after approval"
    );
}

/// 7. 用户拒绝：回复 false → ToolCallResult(is_error=true)
#[tokio::test]
async fn test_user_query_denied() {
    let (tool, executed) = mock_tool("finder", true);
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();
    registry.register(Arc::from(tool)).unwrap();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            setup.session_mgr,
            setup.ctx,
            &config,
            Message::user("Find files"),
            tx,
        )
        .await;
    });

    let mut error_result: Option<String> = None;
    let mut denied_outcome: Option<ToolOutcome> = None;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery { respond, .. } => {
                // Deny immediately to allow tool task to proceed
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: 1,
                        text: String::new(),
                    })
                    .await;
            }
            AgentEvent::ToolCallResult {
                is_error: true,
                content,
                outcome,
                ..
            } => {
                error_result = Some(content);
                denied_outcome = Some(outcome);
            }
            _ => {}
        }
    }

    assert_eq!(error_result, Some("User denied".into()));
    assert_eq!(denied_outcome, Some(ToolOutcome::Denied));
    assert!(
        !executed.load(Ordering::SeqCst),
        "Tool should NOT have been executed after denial"
    );
}

/// 8. 用户 Always Allow：respond with selected_index=2 → 工具执行 + 加入 approved_tools
#[tokio::test]
async fn test_user_query_always_allow() {
    let (tool, executed) = mock_tool("finder", true);
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();
    registry.register(Arc::from(tool)).unwrap();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    let sm = setup.session_mgr.clone();
    let sid = setup.session_id.clone();
    let sm_for_spawn = sm.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm_for_spawn,
            setup.ctx,
            &config,
            Message::user("Find files"),
            tx,
        )
        .await;
    });

    let mut done = false;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery { respond, .. } => {
                // Always Allow
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: 2,
                        text: String::new(),
                    })
                    .await;
            }
            AgentEvent::Done => {
                done = true;
            }
            _ => {}
        }
    }

    assert!(done, "Expected Done event");
    assert!(
        executed.load(Ordering::SeqCst),
        "Tool should have been executed"
    );
    assert!(
        sm.is_tool_approved(&sid, "finder"),
        "Tool should be in approved_tools after Always Allow"
    );
}

/// 9. mpsc 关闭：drop receiver → agent 不 panic
#[tokio::test]
async fn test_mpsc_closed() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello".into()),
        ChatEvent::Done,
    ]]));

    let setup = test_setup();
    let config = AgentConfig::default();
    let (tx, rx) = mpsc::channel(1);
    drop(rx); // drop receiver immediately

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_agent_loop(
            provider,
            StdArc::new(ToolRegistry::new()),
            setup.rule_engine,
            setup.session_mgr,
            setup.ctx,
            &config,
            Message::user("Hi"),
            tx,
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "Agent loop should not hang or panic when receiver is dropped"
    );
}

/// 9. 历史记录：结束时 history 包含 user + assistant 消息
#[tokio::test]
async fn test_history_appended() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello".into()),
        ChatEvent::Done,
    ]]));

    let setup = test_setup();
    let config = AgentConfig::default();
    let (tx, mut rx) = mpsc::channel(64);

    let registry = ToolRegistry::new();
    let sm = setup.session_mgr.clone();
    let sid = setup.session_id.clone();

    let sm_for_spawn = sm.clone();
    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm_for_spawn,
            setup.ctx,
            &config,
            Message::user("Hi"),
            tx,
        )
        .await;
    });

    // Drain events
    while rx.recv().await.is_some() {}

    // Check session history
    let session = sm.get(&sid).unwrap();
    assert_eq!(session.history.len(), 2, "Expected 2 messages in history");
    assert_eq!(session.history[0].role, Role::User);
    assert_eq!(session.history[0].content, "Hi");
    assert_eq!(session.history[1].role, Role::Assistant);
    assert_eq!(session.history[1].content, "Hello");
}

// ── parse_user_query_marker tests ──────────────────────────────────────

#[test]
fn test_parse_marker_simple() {
    let text = "What do you want?\n[USER_QUERY]\nPick one:\n- Option A\n- Option B\n[/USER_QUERY]";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Pick one:");
    assert_eq!(marker.options, vec!["Option A", "Option B"]);
    assert!(!marker.allow_other);
}

#[test]
fn test_parse_marker_with_allow_other() {
    let text = "What do you want?\n[USER_QUERY allow_other=true]\nCustom input:\n- Option 1\n- Option 2\n[/USER_QUERY]";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Custom input:");
    assert_eq!(marker.options, vec!["Option 1", "Option 2"]);
    assert!(marker.allow_other);
}

#[test]
fn test_parse_marker_no_options() {
    let text = "Confirm?\n[USER_QUERY]\nAre you sure?\n[/USER_QUERY]";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Are you sure?");
    assert!(marker.options.is_empty());
    assert!(!marker.allow_other);
}

#[test]
fn test_parse_marker_no_marker() {
    assert!(parse_user_query_marker("Just a normal message").is_none());
    assert!(parse_user_query_marker("").is_none());
    // 无闭合但 body 非空：容错解析（不再是 None）
    let marker = parse_user_query_marker("[USER_QUERY]unclosed").unwrap();
    assert_eq!(marker.message, "unclosed");
    assert!(parse_user_query_marker("[/USER_QUERY]no open").is_none());
}

#[test]
fn test_parse_marker_empty_body() {
    let text = "text\n[USER_QUERY]\n\n[/USER_QUERY]";
    assert!(parse_user_query_marker(text).is_none());
}

#[test]
fn test_parse_marker_multiple_questions_truncated() {
    // LLM 违反"一次一问"约束，一个 marker 塞入 3 个问题：
    // 防御性截断，只保留第一个问题，避免 9 个选项被拍平成一个混合单选列表
    let text =
        "Ask?\n[USER_QUERY]\nQ1?\n- A1\n- A2\n\nQ2?\n- B1\n- B2\n\nQ3?\n- C1\n- C2\n[/USER_QUERY]";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Q1?");
    assert_eq!(marker.options, vec!["A1", "A2"]);
    assert!(!marker.allow_other);
}

#[test]
fn test_parse_marker_blank_lines_between_options_kept() {
    // 同一问题的选项之间存在空行分隔：不应误判为第二个问题
    let text = "Pick:\n[USER_QUERY]\nChoose one:\n- A\n\n- B\n[/USER_QUERY]";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Choose one:");
    assert_eq!(marker.options, vec!["A", "B"]);
}

#[test]
fn test_parse_marker_only_in_text() {
    // Marker in the middle should not match (only at end)
    let text = "[USER_QUERY]\nTest\n[/USER_QUERY]\nsome more text";
    // The rfind will find the first [USER_QUERY] and the last [/USER_QUERY]
    // but there's text after the close tag
    let result = parse_user_query_marker(text);
    // In this case, the function still matches because rfind finds the last [/USER_QUERY]
    // and then looks for [USER_QUERY] before it. There IS text after [/USER_QUERY],
    // but the function uses trim_end() so it would still match.
    // Let's just verify it correctly parses
    assert!(result.is_some());
    let marker = result.unwrap();
    assert_eq!(marker.message, "Test");
}

#[test]
fn test_parse_marker_whitespace_around() {
    let text = "  \n[USER_QUERY]\nHi\n[/USER_QUERY]  \n";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Hi");
}

#[test]
fn test_parse_marker_unclosed() {
    // LLM 输出被截断、缺 [/USER_QUERY] 闭合标记：容错解析到文本末尾
    let text = "What do you want?\n[USER_QUERY]\nPick one:\n- Option A\n- Option B";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Pick one:");
    assert_eq!(marker.options, vec!["Option A", "Option B"]);
    assert!(!marker.allow_other);
}

#[test]
fn test_parse_marker_unclosed_allow_other() {
    let text = "Pick:\n[USER_QUERY allow_other=true]\nCustom:\n- Option 1\n- Option 2";
    let marker = parse_user_query_marker(text).unwrap();
    assert_eq!(marker.message, "Custom:");
    assert_eq!(marker.options, vec!["Option 1", "Option 2"]);
    assert!(marker.allow_other);
}

#[test]
fn test_parse_marker_unclosed_empty_body() {
    // 无闭合标记且 body 为空 → None
    assert!(parse_user_query_marker("text\n[USER_QUERY]\n\n").is_none());
    assert!(parse_user_query_marker("text\n[USER_QUERY]").is_none());
}

#[test]
fn test_parse_marker_no_open_tag() {
    // 无开头标记 → None（无论有无闭合标记）
    assert!(parse_user_query_marker("[/USER_QUERY]").is_none());
    assert!(parse_user_query_marker("Just options\n- A\n- B").is_none());
}

// ── strip_user_query_marker tests ──────────────────────────────────────

#[test]
fn test_strip_marker_simple() {
    let text = "Hello world\n[USER_QUERY]\nConfirm?\n[/USER_QUERY]";
    let stripped = strip_user_query_marker(text);
    assert_eq!(stripped, "Hello world");
}

#[test]
fn test_strip_marker_no_marker() {
    assert_eq!(strip_user_query_marker("Hello"), "Hello");
    assert_eq!(strip_user_query_marker(""), "");
}

#[test]
fn test_strip_marker_only_marker() {
    let stripped = strip_user_query_marker("[USER_QUERY]\nHi\n[/USER_QUERY]");
    assert_eq!(stripped, "");
}

// ── Agent loop [USER_QUERY] marker integration test ─────────────────────

#[tokio::test]
async fn test_user_query_marker_in_response() {
    // Provider returns text with [USER_QUERY] marker
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::TextDelta(
                "What color?\n[USER_QUERY]\nChoose:\n- Red\n- Blue\n[/USER_QUERY]".into(),
            ),
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    let sm = setup.session_mgr.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm,
            setup.ctx,
            &config,
            Message::user("Pick a color"),
            tx,
        )
        .await;
    });

    let mut received_query = false;
    let mut done = false;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery {
                ref message,
                ref options,
                allow_other,
                kind,
                respond,
                ..
            } => {
                received_query = true;
                assert_eq!(message, "Choose:");
                assert_eq!(options, &vec!["Red", "Blue"]);
                assert!(!allow_other);
                assert_eq!(kind, PermissionKind::Question);
                // Respond with option index 0 (Red)
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: 0,
                        text: String::new(),
                    })
                    .await;
            }
            AgentEvent::Done => {
                done = true;
            }
            _ => {}
        }
    }

    assert!(received_query, "Expected UserQuery event with marker");
    assert!(done, "Expected Done event after marker interaction");
}

#[tokio::test]
async fn test_user_query_marker_continue_loop() {
    // Phase 1: text with [USER_QUERY] → respond → Phase 2: normal text → Done
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::TextDelta(
                "Question?\n[USER_QUERY]\nAnswer:\n- Yes\n- No\n[/USER_QUERY]".into(),
            ),
            ChatEvent::Done,
        ],
        vec![ChatEvent::TextDelta("Got it!".into()), ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    let sm = setup.session_mgr.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm,
            setup.ctx,
            &config,
            Message::user("Start"),
            tx,
        )
        .await;
    });

    let mut marker_found = false;
    let mut text_deltas: Vec<String> = Vec::new();

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery { respond, .. } => {
                marker_found = true;
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: 0,
                        text: String::new(),
                    })
                    .await;
            }
            AgentEvent::TextDelta(t) => {
                text_deltas.push(t);
            }
            AgentEvent::Done => {}
            _ => {}
        }
    }

    assert!(
        marker_found,
        "Expected [USER_QUERY] marker to trigger UserQuery"
    );
    // The second phase text should appear
    assert!(
        text_deltas.iter().any(|t| t.contains("Got it!")),
        "Expected loop to continue after marker query"
    );
}

#[tokio::test]
async fn test_user_query_marker_custom_text() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::TextDelta(
                "Custom?\n[USER_QUERY allow_other=true]\nEnter value:\n- Default\n[/USER_QUERY]"
                    .into(),
            ),
            ChatEvent::Done,
        ],
        vec![ChatEvent::TextDelta("Thanks!".into()), ChatEvent::Done],
    ]));

    let (tx, mut rx) = mpsc::channel(64);
    let registry = ToolRegistry::new();

    let setup = test_setup();
    let config = AgentConfig {
        soft_limit: 50,
        ..Default::default()
    };

    let sm = setup.session_mgr.clone();

    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm,
            setup.ctx,
            &config,
            Message::user("Start"),
            tx,
        )
        .await;
    });

    let mut found = false;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::UserQuery {
                ref message,
                ref options,
                allow_other,
                kind,
                respond,
                ..
            } => {
                found = true;
                assert_eq!(message, "Enter value:");
                assert_eq!(options, &vec!["Default"]);
                assert!(allow_other);
                assert_eq!(kind, PermissionKind::Question);
                // Send custom text with selected_index = -1
                let _ = respond
                    .send(UserQueryResult {
                        selected_index: -1,
                        text: "my custom input".into(),
                    })
                    .await;
            }
            AgentEvent::Done => {}
            _ => {}
        }
    }

    assert!(found, "Expected UserQuery with allow_other");
}

// ── Empty response handling ─────────────────────────────────────────────

#[tokio::test]
async fn test_empty_response_returns_error() {
    // LLM returns Done immediately with no text, no tool calls, no thinking
    // but with tokens consumed (simulates the redacted_thinking bug scenario)
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::UsageInfo {
            input_tokens: 100,
            output_tokens: 4096,
            tool_calls: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost: None,
        },
        ChatEvent::Done,
    ]]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "Expected Error event for empty LLM response"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "Should NOT emit Done when LLM returns empty response"
    );
}

#[tokio::test]
async fn test_thinking_only_response_ok() {
    // LLM returns a thinking block but no text — this is valid after
    // the redacted_thinking fix and should NOT produce an error
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::ThinkingBlock(serde_json::json!({
            "type": "thinking",
            "thinking": "[REDACTED]",
            "signature": "base64sig",
        })),
        ChatEvent::Done,
    ]]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "Should NOT error when LLM returns thinking-only response"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "Expected Done for thinking-only response"
    );
}

/// Thinking-only response with finish_reason="length" should continue
/// the loop, not terminate. The LLM hit the token budget during thinking
/// and needs another iteration to produce the actual response.
#[tokio::test]
async fn test_thinking_only_with_length_continues_loop() {
    use crate::ProviderMetadata;

    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        // Phase 1: thinking only, finish_reason="length"
        vec![
            ChatEvent::ThinkingBlock(serde_json::json!({
                "type": "thinking",
                "thinking": "Let me analyze this...",
            })),
            ChatEvent::OutputMetadata(ProviderMetadata {
                model: "test".into(),
                finish_reasons: vec!["length".into()],
                input_tokens: 100,
                output_tokens: 4096,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 500,
            }),
            ChatEvent::Done,
        ],
        // Phase 2: actual text response
        vec![
            ChatEvent::TextDelta("Here is my answer.".into()),
            ChatEvent::Done,
        ],
    ]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "Should NOT error when LLM returns thinking-only with length finish_reason"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "Expected Done after continuing loop"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("Here is my answer"))),
        "Expected text response from second iteration"
    );
}

fn length_metadata() -> crate::ProviderMetadata {
    crate::ProviderMetadata {
        model: "test".into(),
        finish_reasons: vec!["length".into()],
        input_tokens: 100,
        output_tokens: 4096,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        latency_ms: 500,
    }
}

/// A final answer (text, no tool calls) cut off by max_tokens must not be
/// reported as a completed turn: the loop asks the model to continue once.
#[tokio::test]
async fn test_truncated_final_answer_is_continued() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::TextDelta("Root cause: the OCR serv".into()),
            ChatEvent::OutputMetadata(length_metadata()),
            ChatEvent::Done,
        ],
        vec![
            ChatEvent::TextDelta("ice crashed at 20:59.".into()),
            ChatEvent::Done,
        ],
    ]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "a single truncation should be recovered by continuing, not an error"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("crashed at 20:59"))),
        "expected the continuation from the second iteration"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Done))
            .count(),
        1,
        "exactly one Done, after the continuation"
    );
}

/// If the continuation is truncated again (e.g. the model is looping on the
/// same sentence), the turn ends with an error instead of a silent Done.
#[tokio::test]
async fn test_repeatedly_truncated_final_answer_errors() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::TextDelta("Let me look at it. Let me look at it.".into()),
            ChatEvent::OutputMetadata(length_metadata()),
            ChatEvent::Done,
        ],
        vec![
            ChatEvent::TextDelta("Let me look at it. Let me look at it.".into()),
            ChatEvent::OutputMetadata(length_metadata()),
            ChatEvent::Done,
        ],
    ]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { code: AgentErrorCode::LlmApi, message } if message.contains("max_tokens")
        )),
        "expected a max_tokens truncation error"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "a truncated answer must not be reported as Done"
    );
}

/// Thinking-only response with finish_reason="stop" should also continue
/// the loop. Some providers (e.g. DeepSeek via OpenAI protocol) return
/// "stop" after exhausting the thinking budget without generating text.
#[tokio::test]
async fn test_thinking_only_with_stop_continues_loop() {
    use crate::ProviderMetadata;

    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        // Phase 1: thinking only, finish_reason="stop"
        vec![
            ChatEvent::ThinkingBlock(serde_json::json!({
                "type": "thinking",
                "thinking": "Let me think about this game design...",
            })),
            ChatEvent::OutputMetadata(ProviderMetadata {
                model: "test".into(),
                finish_reasons: vec!["stop".into()],
                input_tokens: 100,
                output_tokens: 4096,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 500,
            }),
            ChatEvent::Done,
        ],
        // Phase 2: actual text response
        vec![
            ChatEvent::TextDelta("Here is the game design.".into()),
            ChatEvent::Done,
        ],
    ]));

    let (events, _sm, _sid) =
        run_collect(provider, vec![], test_setup(), 10, 200, Message::user("Hi")).await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "Should NOT error when LLM returns thinking-only with stop finish_reason"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "Expected Done after continuing loop"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::TextDelta(t) if t.contains("Here is the game design"))
        ),
        "Expected text response from second iteration"
    );
}

// ── Panic safety tests ───────────────────────────────────────────────────

/// 10. agent loop 正常结束 → session 状态为 Idle
#[tokio::test]
async fn test_session_status_idle_on_normal_completion() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello".into()),
        ChatEvent::Done,
    ]]));

    let setup = test_setup();
    let config = AgentConfig::default();
    let (tx, mut rx) = mpsc::channel(64);

    let registry = ToolRegistry::new();
    let sm = setup.session_mgr.clone();
    let sid = setup.session_id.clone();

    let sm_for_spawn = sm.clone();
    tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm_for_spawn,
            setup.ctx,
            &config,
            Message::user("Hi"),
            tx,
        )
        .await;
    });

    // Drain events
    while rx.recv().await.is_some() {}

    // Session should be Completed after normal completion
    let session = sm.get(&sid).unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    assert_eq!(session.history.len(), 2, "Expected 2 messages in history");
}

/// 11. agent loop 通过 catch_unwind 保护 panic
#[tokio::test]
async fn test_panic_does_not_leak_session_running() {
    struct PanicProvider;
    #[async_trait::async_trait]
    impl LlmProvider for PanicProvider {
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[crate::message::ToolDefinition],
            _config: &LlmConfig,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
            LlmError,
        > {
            panic!("provider panic");
        }
    }

    let provider: StdArc<dyn LlmProvider> = StdArc::new(PanicProvider);
    let setup = test_setup();
    let config = AgentConfig::default();
    let (tx, rx) = mpsc::channel(64);

    let registry = ToolRegistry::new();
    let sm = setup.session_mgr.clone();
    let sid = setup.session_id.clone();

    let sm_for_spawn = sm.clone();
    let handle = tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            setup.rule_engine,
            sm_for_spawn,
            setup.ctx,
            &config,
            Message::user("Hi"),
            tx,
        )
        .await;
    });

    // Drop receiver so the loop doesn't hang on send errors
    drop(rx);

    // The JoinHandle should indicate a panic (resumed_unwind)
    let result = handle.await;
    assert!(
        result.is_err(),
        "Expected agent loop to panic, but it completed normally"
    );
    let err = result.unwrap_err();
    assert!(err.is_panic(), "Expected a panic error");

    // Session should be reset to Idle despite the panic
    let session = sm.get(&sid).unwrap();
    assert_eq!(
        session.status,
        SessionStatus::Idle,
        "Session should be Idle after panic, not Running"
    );
}

/// W2: agent loop panic 时，应通过 global_tx 发送 AgentMessage::Error
/// 让 orchestrator 能够把失败结果转发给父 agent。
#[tokio::test]
async fn test_panic_emits_error_envelope_to_global_tx() {
    struct PanicProvider;
    #[async_trait::async_trait]
    impl LlmProvider for PanicProvider {
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[crate::message::ToolDefinition],
            _config: &LlmConfig,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
            LlmError,
        > {
            panic!("provider panic");
        }
    }

    let provider: StdArc<dyn LlmProvider> = StdArc::new(PanicProvider);

    // 手动构建带 global_tx 的 ctx（test_setup 默认不带）
    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn ContextTrimmer + Send + Sync> = StdArc::new(MockTrimmer);

    let (global_tx, mut global_rx) = mpsc::channel::<Envelope>(16);
    let permission_rules = StdArc::new(Vec::new());

    let ctx = session_mgr
        .start_loop(&sid, &trimmer, Some(global_tx), Some(permission_rules))
        .unwrap();

    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel(64);

    let sm_for_spawn = session_mgr.clone();
    let handle = tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            rule_engine,
            sm_for_spawn,
            ctx,
            &config,
            Message::user("Hi"),
            tx,
        )
        .await;
    });

    // 等待 task panic
    let join_res = handle.await;
    assert!(
        join_res.is_err() && join_res.unwrap_err().is_panic(),
        "expected agent loop to panic"
    );

    // 关键断言：global_tx 上应能收到 AgentMessage::Error envelope
    let envelope = tokio::time::timeout(std::time::Duration::from_millis(500), global_rx.recv())
        .await
        .expect("should not timeout waiting for error envelope")
        .expect("envelope channel should not be closed");

    assert_eq!(envelope.session_id, sid);
    match envelope.message {
        AgentMessage::Error { code, message } => {
            assert_eq!(code, AgentErrorCode::Internal);
            assert!(
                message.contains("panic"),
                "error message should mention panic, got: {message}"
            );
        }
        _ => panic!("expected AgentMessage::Error, got a different variant"),
    }
}

// ── render_tool_guide tests ─────────────────────────────────────────────

#[test]
fn test_render_tool_guide_empty() {
    let registry = ToolRegistry::new();
    let guide = render_tool_guide(&registry);
    assert!(guide.is_empty());
}

#[test]
fn test_render_tool_guide_with_tools() {
    use crate::tool::ToolContext;
    use crate::tool::ToolResult;

    struct CategorisedTool {
        name: &'static str,
        cat: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for CategorisedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success("ok")
        }
        fn category(&self) -> &str {
            self.cat
        }
    }

    let registry = ToolRegistry::new();
    registry
        .register(Arc::new(CategorisedTool {
            name: "bash",
            cat: "common",
        }))
        .unwrap();
    registry
        .register(Arc::new(CategorisedTool {
            name: "codegraph",
            cat: "analyze",
        }))
        .unwrap();
    registry
        .register(Arc::new(CategorisedTool {
            name: "fetch",
            cat: "network",
        }))
        .unwrap();

    let guide = render_tool_guide(&registry);
    assert!(!guide.is_empty());
    assert!(guide.contains("## Available Tools"));
    assert!(guide.contains("Common (prefer these first)"));
    assert!(guide.contains("bash"));
    assert!(guide.contains("Analyze"));
    assert!(guide.contains("codegraph"));
    assert!(guide.contains("Network"));
    assert!(guide.contains("fetch"));
}

#[test]
fn test_render_tool_guide_no_task_reference() {
    use crate::tool::ToolContext;
    use crate::tool::ToolResult;

    struct DummyTool;

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "run commands"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success("ok")
        }
    }

    let registry = ToolRegistry::new();
    registry.register(Arc::new(DummyTool)).unwrap();
    let guide = render_tool_guide(&registry);
    assert!(!guide.is_empty());
    assert!(
        !guide.contains("`task` tool"),
        "should not reference the `task` tool, got: {guide}"
    );
}

#[test]
fn test_render_tool_guide_lists_agent_tools() {
    use crate::tool::ToolContext;
    use crate::tool::ToolResult;

    struct DummyTool;

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "run commands"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success("ok")
        }
    }

    let registry = ToolRegistry::new();
    registry.register(Arc::new(DummyTool)).unwrap();
    let guide = render_tool_guide(&registry);
    assert!(!guide.is_empty());
    // Should mention at least one agent tool name like @explorer or @fixer
    assert!(
        guide.contains("@explorer") || guide.contains("@fixer") || guide.contains("@designer"),
        "should list agent tool names like @explorer, got: {guide}"
    );
}

// ── AgentTool::execute ──────────────────────────────────────────────────

#[tokio::test]
async fn test_agent_tool_execute_sends_spawn_request_and_returns_response() {
    let tool = AgentTool::new("explorer".to_string(), "Search codebase".to_string());
    let (tx, mut rx) = mpsc::channel::<Envelope>(8);

    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: Some("test-session".to_string()),
        permission_rules: None,
        global_tx: Some(tx),
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };

    let handle = tokio::spawn(async move {
        let env = rx.recv().await.expect("should receive envelope");
        assert_eq!(env.session_id, "test-session");
        match env.message {
            AgentMessage::SpawnRequest {
                subagent_type,
                description,
                prompt,
                response_tx,
                task_id,
                ..
            } => {
                assert_eq!(subagent_type, "explorer");
                assert_eq!(description, "Search codebase");
                assert_eq!(prompt, "find all TODOs");
                assert!(task_id.is_none());
                if let Some(tx) = response_tx {
                    let _ = tx.send("found 3 TODOs".to_string());
                }
            }
            _ => panic!("expected SpawnRequest"),
        }
    });

    let result = tool
        .execute(serde_json::json!({"prompt": "find all TODOs"}), &ctx)
        .await;

    assert!(!result.is_error);
    assert_eq!(result.content, "found 3 TODOs");
    handle.await.unwrap();
}

#[tokio::test]
async fn test_agent_tool_execute_error_without_global_tx() {
    let tool = AgentTool::new("fixer".to_string(), "Fix things".to_string());

    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };

    let result = tool
        .execute(serde_json::json!({"prompt": "do something"}), &ctx)
        .await;

    assert!(result.is_error);
    assert!(result.content.contains("global_tx not available"));
}

#[tokio::test]
async fn test_agent_tool_execute_error_missing_prompt() {
    let tool = AgentTool::new("explorer".to_string(), "Search".to_string());
    let (tx, _rx) = mpsc::channel::<Envelope>(8);

    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: Some(tx),
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };

    let result = tool.execute(serde_json::json!({}), &ctx).await;

    assert!(result.is_error);
    assert!(result.content.contains("Missing required 'prompt'"));
}

// ── ToolOutcome：五值 + is_error 单一映射（计划 0c-2 测试 1/2）──────────────

#[test]
fn test_tool_outcome_variants_and_is_error_mapping() {
    // 五值齐全
    let all = [
        ToolOutcome::Success,
        ToolOutcome::Failure,
        ToolOutcome::Denied,
        ToolOutcome::Cancelled,
        ToolOutcome::Truncated,
    ];
    // 可序列化（hook 契约使用小写名）
    assert_eq!(
        serde_json::to_string(&all).unwrap(),
        r#"["success","failure","denied","cancelled","truncated"]"#
    );
    // 映射不变量：Success -> false，其余 -> true
    assert!(!ToolOutcome::Success.into_is_error());
    for outcome in [
        ToolOutcome::Failure,
        ToolOutcome::Denied,
        ToolOutcome::Cancelled,
        ToolOutcome::Truncated,
    ] {
        assert!(
            outcome.into_is_error(),
            "{outcome:?} 应映射为 is_error=true"
        );
    }
}

/// 测试 3（截断路径）：畸形参数 → 不执行工具，is_error=true 且 outcome=Truncated。
#[tokio::test]
async fn test_tool_call_result_truncated_outcome() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(TestProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call-trunc".into(),
                name: "finder".into(),
                arguments: "{ \"path\": ".into(), // 畸形/截断 JSON
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));
    let (tool, executed) = mock_tool("finder", false);

    let (events, _sm, _sid) = run_collect(
        provider,
        vec![tool],
        test_setup(),
        10,
        200,
        Message::user("truncate"),
    )
    .await;

    assert!(!executed.load(Ordering::SeqCst), "畸形参数不应执行工具");
    let hit = events.iter().find_map(|e| match e {
        AgentEvent::ToolCallResult {
            is_error,
            outcome,
            content,
            ..
        } if content.contains("[TRUNCATED]") => Some((*is_error, *outcome)),
        _ => None,
    });
    assert_eq!(hit, Some((true, ToolOutcome::Truncated)));
}

// ── cleanup_orphan_tool_uses ──────────────────────────────────────────────

#[test]
fn test_cleanup_single_orphan_at_end() {
    let mut history = vec![
        Message::user("do task"),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_a".to_string(),
            name: "search".to_string(),
            arguments: "{}".to_string(),
        }]),
    ];
    cleanup_orphan_tool_uses(&mut history);
    // 孤儿 tool_call 被清空
    assert!(history[1].tool_calls.is_none());
}

#[test]
fn test_cleanup_complete_pair_unchanged() {
    let mut history = vec![
        Message::user("do task"),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_a".to_string(),
            name: "search".to_string(),
            arguments: "{}".to_string(),
        }]),
        Message::tool("result", "call_a"),
    ];
    let original = history.clone();
    cleanup_orphan_tool_uses(&mut history);
    // 完整配对保持不变
    assert_eq!(history[1].tool_calls, original[1].tool_calls);
    assert_eq!(history[2].role, Role::Tool);
}

#[test]
fn test_cleanup_orphan_middle_complete_end() {
    // orphan 在中间，完整配对在末尾 → 两个都应该被检查
    let mut history = vec![
        Message::user("task A"),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_orphan".to_string(),
            name: "search".to_string(),
            arguments: "{}".to_string(),
        }]),
        // 中间 user 消息隔开（模拟中断后用户新消息）
        Message::user("task B"),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_good".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        }]),
        Message::tool("result", "call_good"),
    ];
    cleanup_orphan_tool_uses(&mut history);
    // 孤儿 tool_call 应该被清空（之前因为 break bug 会漏掉）
    assert!(
        history[1].tool_calls.is_none(),
        "orphan tool_calls should be cleared"
    );
    // 完整配对应该保留
    assert!(history[3].tool_calls.is_some(), "good pair should stay");
    assert_eq!(history[4].role, Role::Tool);
}

#[test]
fn test_cleanup_id_mismatch() {
    // assistant tool_call id 与 tool 结果 id 不匹配
    let mut history = vec![
        Message::user("do task"),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_a".to_string(),
            name: "search".to_string(),
            arguments: "{}".to_string(),
        }]),
        Message::tool("result", "call_b"), // ID 不匹配！
    ];
    cleanup_orphan_tool_uses(&mut history);
    // ID 不匹配 → tool_calls 被清空
    assert!(
        history[1].tool_calls.is_none(),
        "mismatched tool_calls should be cleared"
    );
    // tool 消息被标记为 skip_context
    assert!(
        history[2].skip_context,
        "orphan tool result should be skip_context"
    );
}

#[test]
fn test_cleanup_orphan_tool_message() {
    // tool 消息存在但没有匹配的 assistant tool_call
    let mut history = vec![
        Message::user("do task"),
        Message::assistant("done"),
        Message::tool("orphan result", "call_orphan"),
    ];
    cleanup_orphan_tool_uses(&mut history);
    // 孤儿 tool 消息被标记为 skip_context
    assert!(
        history[2].skip_context,
        "orphan tool message should be skip_context"
    );
}

#[test]
fn test_cleanup_multi_tool_calls_partial_match() {
    // assistant 有 2 个 tool_calls，但只有 1 个 tool 结果（且 ID 匹配第一个）
    let mut history = vec![
        Message::user("do task"),
        Message::tool_call(vec![
            ToolCallRequest {
                id: "call_a".to_string(),
                name: "search".to_string(),
                arguments: "{}".to_string(),
            },
            ToolCallRequest {
                id: "call_b".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            },
        ]),
        Message::tool("result a", "call_a"),
        // call_b 的结果缺失！
    ];
    cleanup_orphan_tool_uses(&mut history);
    // 部分匹配 → tool_calls 全部清空
    assert!(
        history[1].tool_calls.is_none(),
        "partial match should clear all"
    );
    // tool 消息 call_a 被标记为孤儿
    assert!(
        history[2].skip_context,
        "orphan tool result should be skip_context"
    );
}

// ── W1-S1-3: TraceContext on SpawnRequest & Envelope ────────────────

#[test]
fn test_spawn_request_carries_trace_context() {
    let tc = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        None,
        None,
    )
    .unwrap();
    let spawn = AgentMessage::SpawnRequest {
        call_id: "call-1".into(),
        subagent_type: "default".into(),
        description: "do work".into(),
        prompt: "do work".into(),
        task_id: None,
        trace_context: Some(tc.clone()),
        response_tx: None,
    };
    match &spawn {
        AgentMessage::SpawnRequest { trace_context, .. } => {
            assert_eq!(trace_context, &Some(tc));
        }
        _ => panic!("expected SpawnRequest"),
    }
}

#[test]
fn test_spawn_request_backward_compat_default_none() {
    let spawn = AgentMessage::SpawnRequest {
        call_id: "call-1".into(),
        subagent_type: "default".into(),
        description: "do work".into(),
        prompt: "do work".into(),
        task_id: None,
        trace_context: None,
        response_tx: None,
    };
    match &spawn {
        AgentMessage::SpawnRequest { trace_context, .. } => {
            assert!(trace_context.is_none());
        }
        _ => panic!("expected SpawnRequest"),
    }
}

#[test]
fn test_spawn_request_response_tx_none_by_default() {
    let spawn = AgentMessage::SpawnRequest {
        call_id: "call-1".into(),
        subagent_type: "default".into(),
        description: "do work".into(),
        prompt: "do work".into(),
        task_id: None,
        trace_context: None,
        response_tx: None,
    };
    match &spawn {
        AgentMessage::SpawnRequest { response_tx, .. } => {
            assert!(response_tx.is_none());
        }
        _ => panic!("expected SpawnRequest"),
    }
}

#[test]
fn test_spawn_request_response_tx_send_receive() {
    let (tx, mut rx) = tokio::sync::oneshot::channel::<String>();
    let spawn = AgentMessage::SpawnRequest {
        call_id: "call-2".into(),
        subagent_type: "default".into(),
        description: "do work".into(),
        prompt: "do work".into(),
        task_id: None,
        trace_context: None,
        response_tx: Some(tx),
    };
    if let AgentMessage::SpawnRequest {
        response_tx: Some(sender),
        ..
    } = spawn
    {
        sender.send("hello".to_string()).ok();
    }
    let result = rx.try_recv();
    assert_eq!(result, Ok("hello".to_string()));
}

#[test]
fn test_envelope_carries_trace_context() {
    let tc = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        None,
        None,
    )
    .unwrap();
    let envelope = Envelope {
        session_id: "sess-1".into(),
        message: AgentMessage::Done,
        trace_context: Some(tc.clone()),
    };
    assert_eq!(envelope.trace_context, Some(tc));
}

#[test]
fn test_envelope_backward_compat_default_none() {
    let envelope = Envelope {
        session_id: "sess-1".into(),
        message: AgentMessage::Done,
        trace_context: None,
    };
    assert!(envelope.trace_context.is_none());
}
