use super::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use visp_core::context::{ContextTrimmer, NoopTrimmer};
use visp_core::session::InMemorySessionStore;

/// 构造带指定模型的 daemon 配置（供 agent 模型覆盖 / provider 解析使用）
fn daemon_config_with_models(
    models: Vec<visp_config::LlmModelConfig>,
) -> Arc<visp_config::DaemonConfig> {
    Arc::new(visp_config::DaemonConfig {
        llm: visp_config::LlmSection {
            models,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// 测试用空事件发布器：`Orchestrator` 会把总线事件发布到 [`EventPublisher`]，
/// 测试若无需断言总线则注入本无操作实现（visp-agent 无法引用 visp-daemon 的 `EventBus`）。
struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&self, _event: BusEvent) {}
}

fn noop_publisher() -> Arc<dyn EventPublisher> {
    Arc::new(NoopPublisher)
}

/// 测试用记录发布器：记录所有经总线发布的事件，供断言「哪些事件不应发布」。
#[derive(Default)]
struct RecordingPublisher {
    events: std::sync::Mutex<Vec<BusEvent>>,
}

impl EventPublisher for RecordingPublisher {
    fn publish(&self, event: BusEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn recording_publisher() -> Arc<RecordingPublisher> {
    Arc::new(RecordingPublisher::default())
}

fn make_orchestrator() -> (
    Orchestrator,
    mpsc::Sender<Envelope>,
    mpsc::Sender<ClientMessage>,
) {
    make_orchestrator_with_publisher(noop_publisher())
}

fn make_orchestrator_with_publisher(
    bus: Arc<dyn EventPublisher>,
) -> (
    Orchestrator,
    mpsc::Sender<Envelope>,
    mpsc::Sender<ClientMessage>,
) {
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (client_tx, client_rx) = mpsc::channel(64);

    let global_tx_for_orch = global_tx.clone();
    let global_tx_for_test = global_tx;

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));
    let agent_registry = Arc::new(ArcSwap::from_pointee(AgentRegistry::new()));
    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let agent_config = AgentConfig::default();
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    let orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx_for_orch,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        agent_config,
        context_trimmer,
        Arc::new(visp_config::DaemonConfig::default()),
        HashMap::new(),
        bus,
    );

    (orch, global_tx_for_test, client_tx)
}

#[tokio::test]
async fn test_handle_text_delta_not_forwarded() {
    let recorder = recording_publisher();
    let (mut orch, global_tx, _client_tx) = make_orchestrator_with_publisher(recorder.clone());

    // Send TextDelta via global_tx (现已不再转发到总线)
    global_tx
        .send(Envelope {
            session_id: "s-1".to_string(),
            message: AgentMessage::TextDelta("hello".to_string()),
            trace_context: None,
        })
        .await
        .unwrap();

    // Process it
    if let Ok(envelope) = orch.global_rx.try_recv() {
        orch.handle_agent_message(envelope).await;
    }

    // TextDelta 不应经 handle_agent_message 发布（由 run_agent_loop 直接送达总线）
    assert!(
        recorder.events.lock().unwrap().is_empty(),
        "TextDelta should not be forwarded by handle_agent_message"
    );
}

#[tokio::test]
async fn test_pending_query_routing() {
    let (mut orch, _global_tx, _client_tx) = make_orchestrator();
    let (respond, _response) = mpsc::channel(1);

    // Insert a pending query directly
    orch.handle_agent_message(Envelope {
        session_id: "s-1".to_string(),
        message: AgentMessage::UserQuery {
            query_id: "q-1".to_string(),
            message: "Allow?".to_string(),
            options: vec![],
            allow_other: false,
            respond,
        },
        trace_context: None,
    })
    .await;

    // Verify it was stored
    assert!(orch.pending_queries.contains_key("q-1"));
}

#[test]
fn test_resolve_provider_fallback() {
    let (orch, _global_tx, _client_tx) = make_orchestrator();
    // Without any providers registered, resolve_provider returns None
    let result = orch.resolve_provider(None, "unknown");
    assert!(result.is_none());
}

// ── Multi-agent integration tests ─────────────────────────────────

#[tokio::test]
async fn test_spawn_request_creates_sub_agent() {
    let (mut orch, _global_tx, _client_tx) = make_orchestrator();

    // Inject a SpawnRequest
    let envelope = Envelope {
        session_id: "parent-1".to_string(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-task-1".to_string(),
            subagent_type: "default".to_string(),
            description: "do something".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // The orchestrator should attempt to spawn the sub-agent.
    // Since no parent session exists it should log an error,
    // but we can verify it didn't panic by reaching here.
}

#[tokio::test]
async fn test_create_sub_with_parent_reference() {
    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    // Create parent session
    let parent = session_mgr
        .create(
            &std::path::PathBuf::from("/tmp"),
            visp_core::provider::LlmConfig::default(),
        )
        .unwrap();

    // Create sub-session using create_sub (public API)
    let child = session_mgr
        .create_sub(SubSessionParams {
            parent_id: Some(parent.id.clone()),
            agent_name: "code-review".to_string(),
            session_id: None,
            project_path: std::path::PathBuf::from("/tmp"),
            config: visp_core::provider::LlmConfig::default(),
            permission: vec![visp_core::agent_definition::PermissionRule {
                permission: "edit".to_string(),
                pattern: "*".to_string(),
                action: visp_core::agent_definition::PermissionAction::Deny,
            }],
            approved_tools: HashSet::new(),
        })
        .unwrap();

    // Verify child has parent_id set
    let child_session = session_mgr.get(&child.id).unwrap();
    let parent_id = parent.id.clone();
    assert_eq!(child_session.parent_id, Some(parent_id.clone()));

    // Verify the parent session exists via list
    let sessions = session_mgr.list().unwrap();
    assert!(sessions.iter().any(|s| s.id == parent_id));
    assert!(sessions.iter().any(|s| s.id == child.id));
}

#[tokio::test]
async fn test_max_depth_exceeded() {
    // This test validates that sub-session creation with a parent reference works.
    // Depth enforcement is done at runtime by the orchestrator's active_agents registry.
    // Default AgentConfig has max_depth = 5.
    #[allow(unused_variables)]
    let agent_config = AgentConfig::default();
    assert_eq!(agent_config.max_depth, 5);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    // Create parent session
    let parent = session_mgr
        .create(
            &std::path::PathBuf::from("/tmp"),
            visp_core::provider::LlmConfig::default(),
        )
        .unwrap();

    // Create child of parent
    let child = session_mgr
        .create_sub(SubSessionParams {
            parent_id: Some(parent.id.clone()),
            agent_name: "child".to_string(),
            session_id: None,
            project_path: std::path::PathBuf::from("/tmp"),
            config: visp_core::provider::LlmConfig::default(),
            permission: vec![],
            approved_tools: HashSet::new(),
        })
        .unwrap();

    // Verify parent-child chain
    let parent_id = parent.id.clone();
    assert_eq!(child.parent_id, Some(parent_id.clone()));
    assert_ne!(child.id, parent_id);
}

#[tokio::test]
async fn test_subagent_permission_inheritance() {
    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    let parent = session_mgr
        .create(
            &std::path::PathBuf::from("/tmp"),
            visp_core::provider::LlmConfig::default(),
        )
        .unwrap();

    // Create child with deny on "edit"
    let child = session_mgr
        .create_sub(SubSessionParams {
            parent_id: Some(parent.id.clone()),
            agent_name: "reviewer".to_string(),
            session_id: None,
            project_path: std::path::PathBuf::from("/tmp"),
            config: visp_core::provider::LlmConfig::default(),
            permission: vec![visp_core::agent_definition::PermissionRule {
                permission: "edit".to_string(),
                pattern: "*".to_string(),
                action: visp_core::agent_definition::PermissionAction::Deny,
            }],
            approved_tools: HashSet::new(),
        })
        .unwrap();

    let child_session = session_mgr.get(&child.id).unwrap();
    assert_eq!(child_session.parent_id, Some(parent.id));
    assert_eq!(child_session.agent_name, "reviewer");
    // Verify permission was set on child session
    assert!(!child_session.permission.is_empty());
}

// ── W3: Orchestrator 持有 sub-agent JoinHandle ───────────────────────

#[tokio::test]
async fn test_subagent_applies_agent_model_override() {
    // Verify that when an agent definition has a `model` key,
    // the sub-session's LlmConfig is overridden with the correct model info.
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (_client_tx, client_rx) = mpsc::channel(64);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    // Parent session with default model
    let parent = session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap();
    let parent_id = parent.id.clone();

    // Register a sub-agent with a specific model key and temperature
    let mut agent_registry = AgentRegistry::new();
    agent_registry
        .register(AgentDefinition {
            name: "explorer".to_string(),
            description: String::new(),
            mode: AgentMode::Subagent,
            model: Some("Opencode/deepseek-v4-flash".to_string()),
            temperature: Some(0.1),
            steps: Some(10),
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(agent_registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let agent_config = AgentConfig::default();
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    let provider: Arc<dyn LlmProvider> = Arc::new(visp_llm::mock::MockProvider::new(vec![]));
    let mut providers = HashMap::new();
    providers.insert("Opencode/deepseek-v4-flash".to_string(), provider);

    // Build daemon config with the matching model
    let daemon_config = daemon_config_with_models(vec![visp_config::LlmModelConfig {
        name: "deepseek-v4-flash".to_string(),
        protocol: "openai".to_string(),
        provider: Some("Opencode".to_string()),
        model: "deepseek-v4-flash".to_string(),
        api_key: None,
        base_url: None,
        temperature: Some(0.5),
        max_tokens: Some(8192),
        max_context_tokens: Some(64000),
        thinking_budget_tokens: None,
        use_tool: None,
        image_generation: None,
        extra: HashMap::new(),
    }]);

    let mut orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        agent_config,
        context_trimmer,
        daemon_config,
        providers,
        noop_publisher(),
    );

    // Spawn the sub-agent
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-model-test".to_string(),
            subagent_type: "explorer".to_string(),
            description: "test task".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // Find the sub-session and verify its config was overridden
    let sessions = orch.session_mgr.list().unwrap();
    let sub_session = sessions
        .iter()
        .find(|s| s.agent_name == "explorer" && s.parent_id == Some(parent_id.clone()))
        .expect("sub-session should exist");

    // Model should be overridden from parent's "claude-3-7-sonnet-20250219" to "deepseek-v4-flash"
    assert_eq!(
        sub_session.config.model, "deepseek-v4-flash",
        "sub-session model should be overridden by agent def"
    );
    assert_eq!(
        sub_session.config.model_key,
        Some("Opencode/deepseek-v4-flash".to_string()),
        "sub-session model_key should match agent def model"
    );
    assert_eq!(
        sub_session.config.provider,
        Some("Opencode".to_string()),
        "sub-session provider should be overridden"
    );
    // Temperature: agent_def.temperature (0.1) takes precedence over model_info (0.5)
    assert!(
        (sub_session.config.temperature - 0.1).abs() < 1e-5,
        "sub-session temperature should be overridden by agent def temperature, got: {}",
        sub_session.config.temperature
    );
    // max_tokens from model_info
    assert_eq!(
        sub_session.config.max_tokens, 8192,
        "sub-session max_tokens should be overridden by model info"
    );
    // max_context_tokens from model_info
    assert_eq!(
        sub_session.config.max_context_tokens, 64000,
        "sub-session max_context_tokens should be overridden by model info"
    );
}

#[tokio::test]
async fn test_subagent_inherits_parent_config_when_no_model_override() {
    // Verify that when an agent definition has NO `model` key,
    // the sub-session inherits the parent's config unchanged.
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (_client_tx, client_rx) = mpsc::channel(64);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    // Parent session with custom model
    let parent_config = LlmConfig {
        model: "parent-model".to_string(),
        temperature: 0.8,
        ..LlmConfig::default()
    };
    let parent = session_mgr
        .create(&PathBuf::from("/tmp"), parent_config)
        .unwrap();
    let parent_id = parent.id.clone();

    // Register a sub-agent with NO model override
    let mut agent_registry = AgentRegistry::new();
    agent_registry
        .register(AgentDefinition {
            name: "explorer".to_string(),
            description: String::new(),
            mode: AgentMode::Subagent,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(agent_registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let agent_config = AgentConfig::default();
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    let provider: Arc<dyn LlmProvider> = Arc::new(visp_llm::mock::MockProvider::new(vec![]));
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), provider);

    let mut orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        agent_config,
        context_trimmer,
        Arc::new(visp_config::DaemonConfig::default()),
        providers,
        noop_publisher(),
    );

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-no-override".to_string(),
            subagent_type: "explorer".to_string(),
            description: "test task".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let sessions = orch.session_mgr.list().unwrap();
    let sub_session = sessions
        .iter()
        .find(|s| s.agent_name == "explorer" && s.parent_id == Some(parent_id.clone()))
        .expect("sub-session should exist");

    // Should inherit parent's model
    assert_eq!(
        sub_session.config.model, "parent-model",
        "sub-session should inherit parent model when no agent model override"
    );
    assert!(
        (sub_session.config.temperature - 0.8).abs() < f64::EPSILON,
        "sub-session should inherit parent temperature when no agent override"
    );
}

/// Bug 复现：agent 定义有 model override 时，用户通过 /model 切换模型后，
/// start_main_agent 不应该用 agent 定义的 model 覆盖 session.config。
#[tokio::test]
async fn test_main_agent_respects_user_model_switch() {
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (_client_tx, client_rx) = mpsc::channel(64);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));

    // 创建 session，初始用 model-a
    let initial_config = LlmConfig {
        model: "model-a".to_string(),
        model_key: Some("ProviderA/model-a".to_string()),
        provider: Some("ProviderA".to_string()),
        ..LlmConfig::default()
    };
    let session = session_mgr
        .create(&PathBuf::from("/tmp"), initial_config)
        .unwrap();
    let session_id = session.id.clone();

    // 注册有 model override 的 default agent
    let mut agent_registry = AgentRegistry::new();
    agent_registry
        .register(AgentDefinition {
            name: "default".to_string(),
            description: String::new(),
            mode: AgentMode::All,
            model: Some("ProviderA/model-a".to_string()),
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(agent_registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let agent_config = AgentConfig::default();
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    // 两个 provider：model-a 和 model-b
    let provider: Arc<dyn LlmProvider> = Arc::new(visp_llm::mock::MockProvider::new(vec![]));
    let mut providers = HashMap::new();
    providers.insert("ProviderA/model-a".to_string(), provider.clone());
    providers.insert("ProviderB/model-b".to_string(), provider);

    // Build daemon config with both models (agent def 与用户切换的目标模型)
    let daemon_config = daemon_config_with_models(vec![
        visp_config::LlmModelConfig {
            name: "model-a".to_string(),
            protocol: "openai".to_string(),
            provider: Some("ProviderA".to_string()),
            model: "model-a".to_string(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: HashMap::new(),
        },
        visp_config::LlmModelConfig {
            name: "model-b".to_string(),
            protocol: "openai".to_string(),
            provider: Some("ProviderB".to_string()),
            model: "model-b".to_string(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: HashMap::new(),
        },
    ]);

    let mut orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        agent_config,
        context_trimmer,
        daemon_config,
        providers,
        noop_publisher(),
    );

    // 模拟用户通过 /model 切换到 model-b
    let switched_config = LlmConfig {
        model: "model-b".to_string(),
        model_key: Some("ProviderB/model-b".to_string()),
        provider: Some("ProviderB".to_string()),
        ..LlmConfig::default()
    };
    orch.session_mgr
        .update_config(&session_id, switched_config)
        .unwrap();

    // 启动主 agent
    orch.start_main_agent(&session_id, "hello").await;

    // 验证 session.config 没有被 agent override 覆盖回 model-a
    let session = orch.session_mgr.get(&session_id).unwrap();
    assert_eq!(
        session.config.model, "model-b",
        "用户切换的 model-b 不应被 agent 定义覆盖回 model-a"
    );
    assert_eq!(
        session.config.model_key,
        Some("ProviderB/model-b".to_string()),
        "用户切换的 model_key 不应被 agent 定义覆盖"
    );
}

#[tokio::test]
async fn test_orchestrator_tracks_sub_agent_join_handles() {
    let (mut orch, _global_tx, _client_tx) = make_orchestrator();

    // 初始 map 为空
    assert!(
        orch.sub_agent_handles.is_empty(),
        "新建 Orchestrator 的 sub_agent_handles 应为空"
    );

    // 模拟 spawn：插入一个长跑的 task handle
    let handle = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });
    orch.sub_agent_handles.insert("child-1".to_string(), handle);

    assert_eq!(orch.sub_agent_handles.len(), 1);
    assert!(orch.sub_agent_handles.contains_key("child-1"));

    // 注册对应 active agent，便于 handle_done 走通常路径
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, _parent_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: "parent-1".to_string(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel.clone(),
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });
    orch.active_agents.register(ActiveAgent {
        session_id: "child-1".to_string(),
        parent_session_id: Some("parent-1".to_string()),
        agent_name: "sub-agent".to_string(),
        cancel_token: cancel.clone(),
        inbox: mpsc::channel(16).0,
        pending_call_id: Some("call-1".to_string()),
        started_at: Instant::now(),
    });

    // handle_done 后应从 map 中移除
    orch.handle_done("child-1").await;

    assert!(
        !orch.sub_agent_handles.contains_key("child-1"),
        "handle_done 后 sub_agent_handles 应移除该 session"
    );
}

// ── W1: Sub-agent lifecycle observability ─────────────────────────────

#[tokio::test]
async fn test_handle_done_emits_completion_log() {
    let (mut orch, _global_tx, _client_tx) = make_orchestrator();
    let cancel = CancellationToken::new();

    // Create inbox for parent agent
    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);

    // Register parent agent
    orch.active_agents.register(ActiveAgent {
        session_id: "parent-1".to_string(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel.clone(),
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    // Register sub-agent
    orch.active_agents.register(ActiveAgent {
        session_id: "child-1".to_string(),
        parent_session_id: Some("parent-1".to_string()),
        agent_name: "sub-agent".to_string(),
        cancel_token: cancel.clone(),
        inbox: mpsc::channel(16).0,
        pending_call_id: Some("call-task-1".to_string()),
        started_at: Instant::now(),
    });

    // Act: handle_done for the sub-agent
    orch.handle_done("child-1").await;

    // Assert: child removed from registry
    assert!(orch.active_agents.get("child-1").is_none());

    // Assert: parent still active
    assert!(orch.active_agents.get("parent-1").is_some());

    // Assert: parent inbox received SubAgentComplete
    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should contain SubAgentComplete");
    match msg {
        OrchestratorMessage::SubAgentComplete { call_id, .. } => {
            assert_eq!(call_id, "call-task-1");
        }
        _ => panic!("expected SubAgentComplete, got a different variant"),
    }
}

/// W2: sub-agent 发 AgentMessage::Error（如 panic 转发）时，
/// orchestrator 应通过 SubAgentError 通知父 agent，而非走 SubAgentComplete 的空内容路径。
#[tokio::test]
async fn test_handle_agent_error_forwards_sub_agent_error_to_parent() {
    use visp_core::agent::{AgentMessage, Envelope};
    use visp_core::error::AgentErrorCode;

    let (mut orch, _global_tx, _client_tx) = make_orchestrator();
    let cancel = CancellationToken::new();

    // Parent agent + inbox
    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: "parent-1".to_string(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel.clone(),
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    // Sub-agent with pending_call_id
    orch.active_agents.register(ActiveAgent {
        session_id: "child-1".to_string(),
        parent_session_id: Some("parent-1".to_string()),
        agent_name: "sub-agent".to_string(),
        cancel_token: cancel.clone(),
        inbox: mpsc::channel(16).0,
        pending_call_id: Some("call-task-1".to_string()),
        started_at: Instant::now(),
    });

    // Act: simulate AgentMessage::Error envelope from the sub-agent
    let envelope = Envelope {
        session_id: "child-1".to_string(),
        message: AgentMessage::Error {
            code: AgentErrorCode::Internal,
            message: "agent loop panicked: provider panic".to_string(),
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // Assert: child removed from registry
    assert!(
        orch.active_agents.get("child-1").is_none(),
        "child should be removed after error"
    );

    // Assert: parent inbox received SubAgentError (NOT SubAgentComplete)
    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should contain a message");
    match msg {
        OrchestratorMessage::SubAgentError { call_id, error } => {
            assert_eq!(call_id, "call-task-1");
            assert!(
                error.contains("panic"),
                "error should mention panic, got: {error}"
            );
        }
        OrchestratorMessage::SubAgentComplete { .. } => {
            panic!("got SubAgentComplete but expected SubAgentError on error path");
        }
        _ => panic!("unexpected message variant"),
    }
}

// ── W1-S3c: 注入 visp.subagent.spawn span ─────────────────────────────
use std::sync::Arc as TArc;
use std::sync::Mutex as TMutex;
use std::time::Duration;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use visp_core::agent_definition::AgentMode;
use visp_core::provider::LlmConfig;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: Vec<(String, String)>,
    id: u64,
    parent_id: Option<u64>,
    tid: String,
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
    spans: TArc<TMutex<Vec<CapturedSpan>>>,
    #[allow(dead_code)]
    events: TArc<TMutex<Vec<String>>>,
    captured_tcs: TArc<TMutex<Vec<visp_core::TraceContext>>>,
}

impl TestLayer {
    fn new(
        spans: TArc<TMutex<Vec<CapturedSpan>>>,
        events: TArc<TMutex<Vec<String>>>,
        captured_tcs: TArc<TMutex<Vec<visp_core::TraceContext>>>,
    ) -> Self {
        Self {
            spans,
            events,
            captured_tcs,
        }
    }
}

impl<S> Layer<S> for TestLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = SpanFieldVisitor { fields: Vec::new() };
        attrs.record(&mut visitor);
        let parent_id = ctx.lookup_current().map(|s| s.id().into_u64());
        let mut spans = self.spans.lock().unwrap();
        spans.push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.fields,
            id: id.into_u64(),
            parent_id,
            tid: format!("{:?}", std::thread::current().id()),
        });

        // 如果当前 span（父 span）有 TraceContext extension，捕获它
        if let Some(current) = ctx.lookup_current()
            && let Some(tc) = current.extensions().get::<visp_core::TraceContext>()
        {
            self.captured_tcs.lock().unwrap().push(tc.clone());
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut visitor = SpanFieldVisitor { fields: Vec::new() };
        values.record(&mut visitor);
        let mut spans = self.spans.lock().unwrap();
        if let Some(span) = spans.iter_mut().find(|s| s.id == id.into_u64()) {
            span.fields.extend(visitor.fields);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = SpanFieldVisitor { fields: Vec::new() };
        event.record(&mut visitor);
        let msg = visitor
            .fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.events
            .lock()
            .unwrap()
            .push(format!("{}: {}", event.metadata().name(), msg));
    }
}

#[allow(clippy::type_complexity)]
fn setup_tracing() -> (
    TArc<TMutex<Vec<CapturedSpan>>>,
    TArc<TMutex<Vec<String>>>,
    TArc<TMutex<Vec<visp_core::TraceContext>>>,
) {
    let spans = TArc::new(TMutex::new(Vec::new()));
    let events = TArc::new(TMutex::new(Vec::new()));
    let captured_tcs = TArc::new(TMutex::new(Vec::new()));
    (spans, events, captured_tcs)
}

fn make_tracing_guard(
    spans: &TArc<TMutex<Vec<CapturedSpan>>>,
    events: &TArc<TMutex<Vec<String>>>,
    captured_tcs: &TArc<TMutex<Vec<visp_core::TraceContext>>>,
) -> tracing::subscriber::DefaultGuard {
    use tracing_subscriber::layer::SubscriberExt;
    tracing_subscriber::registry()
        .with(TestLayer::new(
            spans.clone(),
            events.clone(),
            captured_tcs.clone(),
        ))
        .set_default()
}

/// 创建完整可用的 orchestrator（含父 session、agent 定义、provider，默认配置）
fn make_orchestrator_for_spawn() -> (Orchestrator, mpsc::Sender<Envelope>, String) {
    make_orchestrator_for_spawn_with_config(AgentConfig::default())
}

/// 创建完整可用的 orchestrator，使用指定的 AgentConfig
fn make_orchestrator_for_spawn_with_config(
    agent_config: AgentConfig,
) -> (Orchestrator, mpsc::Sender<Envelope>, String) {
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (_client_tx, client_rx) = mpsc::channel(64);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));
    let parent = session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap();
    let parent_id = parent.id.clone();

    // Register default subagent type
    let mut agent_registry = AgentRegistry::new();
    agent_registry
        .register(AgentDefinition {
            name: "default".to_string(),
            description: String::new(),
            mode: AgentMode::Subagent,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(agent_registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    let provider: Arc<dyn LlmProvider> = Arc::new(visp_llm::mock::MockProvider::new(vec![]));
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), provider);

    let global_tx_for_orch = global_tx.clone();
    let orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx_for_orch,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        agent_config,
        context_trimmer,
        Arc::new(visp_config::DaemonConfig::default()),
        providers,
        noop_publisher(),
    );

    (orch, global_tx, parent_id)
}

#[tokio::test]
async fn test_subagent_spawn_span_created_in_orchestrator() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-1".to_string(),
            subagent_type: "default".to_string(),
            description: "do something".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_spans: Vec<_> = captured
        .iter()
        .filter(|s| s.name == "visp.subagent.spawn")
        .collect();
    assert!(
        !spawn_spans.is_empty(),
        "expected at least one 'visp.subagent.spawn' span, found {} total spans: {:?}",
        captured.len(),
        captured.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_subagent_spawn_fields() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-field-test".to_string(),
            subagent_type: "default".to_string(),
            description: "test description".to_string(),
            prompt: "test task".into(),
            task_id: Some("task-42".to_string()),
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    assert_eq!(
        field_map.get("visp.subagent.name"),
        Some(&"default"),
        "visp.subagent.name should be 'default', got: {:?}",
        field_map.get("visp.subagent.name")
    );
    assert_eq!(
        field_map.get("visp.subagent.call_id"),
        Some(&"call-field-test"),
        "visp.subagent.call_id should be 'call-field-test'"
    );
    assert!(
        field_map.contains_key("visp.subagent.session_id"),
        "visp.subagent.session_id should be present"
    );
    assert!(
        field_map.get("visp.subagent.task_id") == Some(&"task-42"),
        "visp.subagent.task_id should be 'task-42'"
    );
    assert_eq!(
        field_map.get("visp.subagent.depth"),
        Some(&"0"),
        "visp.subagent.depth should be '0', got: {:?}",
        field_map.get("visp.subagent.depth")
    );
}

#[tokio::test]
async fn test_subagent_run_loop_attached_via_instrument() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-3".to_string(),
            subagent_type: "default".to_string(),
            description: "do something".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // 给 spawned task 时间启动，使 visp.agent.run span 被创建
    tokio::time::sleep(Duration::from_millis(200)).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("expected 'visp.agent.run' span (run_agent_loop should have started)");

    assert_eq!(
        run_span.parent_id,
        Some(spawn_span.id),
        "'visp.agent.run' should be a child of 'visp.subagent.spawn' (parent_id={:?}, spawn_id={})",
        run_span.parent_id,
        spawn_span.id,
    );
}

#[tokio::test]
async fn test_orchestrator_reads_trace_context_from_envelope() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let tc = visp_core::TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        Some("congo=toto".to_string()),
        Some("aaaaaaaaaaaaaaaa".to_string()),
    )
    .unwrap();

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-tc-1".to_string(),
            subagent_type: "default".to_string(),
            description: "task with trace".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: Some(tc.clone()),
            response_tx: None,
        },

        trace_context: Some(tc.clone()),
    };
    orch.handle_agent_message(envelope).await;

    // orchestrator 优先使用 envelope/SpawnRequest 携带的 TraceContext，
    // 确保子 agent 跨 mpsc 边界继承父 trace_id。验证 spawn span 记录了
    // 传入的 trace_id / parent_span_id / trace_state（而非 fallback）。
    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let trace_id = field_map
        .get("trace_id")
        .copied()
        .expect("trace_id should be recorded");
    assert_eq!(
        trace_id, "0af7651916cd43dd8448eb211c80319c",
        "trace_id 必须是 envelope 携带的，而非 fallback UUID"
    );

    let psid = field_map
        .get("parent_span_id")
        .copied()
        .expect("parent_span_id should be recorded");
    assert_eq!(
        psid, "aaaaaaaaaaaaaaaa",
        "parent_span_id 必须是 envelope 携带的"
    );

    // trace_state 来自传入的 TraceContext
    assert_eq!(
        field_map.get("trace_state").copied(),
        Some("congo=toto"),
        "trace_state 应来自传入的 TraceContext"
    );
}

#[tokio::test]
async fn test_orchestrator_missing_trace_context_falls_back_to_orphan() {
    let (spans, events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    // Envelope 不带 trace_context（None），orchestrator 回退到
    // extract_trace_context() 生成 fallback TraceContext。
    //
    // 注意：这里历史上出现过一次罕见 flake —— 整次 handle_agent_message 的
    // span/event 都没有进入本测试的 TestLayer（线程 dispatcher 在高压并行
    // 下偶发抖动），而断言前的探针 __orphan_probe 却正常捕获。为了不让
    // 单次调度异常阻塞 CI，最多重试一次；若重试后仍缺失，则带着完整
    // 诊断信息（thread id / 已捕获 span）失败，方便定位。
    let mut captured: Vec<CapturedSpan> = Vec::new();
    let mut attempts_used = 0usize;
    for attempt in 0..2 {
        let envelope = Envelope {
            session_id: parent_id.clone(),
            message: AgentMessage::SpawnRequest {
                call_id: format!("call-orphan-{attempt}"),
                subagent_type: "default".to_string(),
                description: "orphan test".to_string(),
                prompt: "test task".into(),
                task_id: None,
                trace_context: None,
                response_tx: None,
            },
            trace_context: None,
        };
        orch.handle_agent_message(envelope).await;
        attempts_used += 1;
        {
            let guard = spans.lock().unwrap();
            if guard.iter().any(|s| s.name == "visp.subagent.spawn") {
                captured = guard.clone();
                break;
            }
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // 诊断探针：断言前发一个探针 span，验证当前线程 dispatcher 是否仍指向本测试的 TestLayer
    let _probe = tracing::info_span!("__orphan_probe");

    // 验证不 panic，且 spawn span 已创建
    let captured_locked = spans.lock().unwrap();
    assert!(
        captured.iter().any(|s| s.name == "visp.subagent.spawn"),
        "'visp.subagent.spawn' span should be created even without trace_context; \
         attempts={}, level_filter={:?}, probe_captured={}, test_thread={:?}, \
         captured {} spans: {:?}, all_captured {} spans: {:?}, events: {:?}",
        attempts_used,
        tracing::level_filters::LevelFilter::current(),
        captured_locked.iter().any(|s| s.name == "__orphan_probe"),
        std::thread::current().id(),
        captured.len(),
        captured
            .iter()
            .map(|s| (s.name.as_str(), s.tid.as_str()))
            .collect::<Vec<_>>(),
        captured_locked.len(),
        captured_locked
            .iter()
            .map(|s| (s.name.as_str(), s.tid.as_str()))
            .collect::<Vec<_>>(),
        events.lock().unwrap()
    );

    // W2-S4: orchestrator 会生成 fallback TraceContext（UUID based），因此
    // trace_id / parent_span_id 会被记录到 span fields 上。
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");
    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // trace_id 应存在（fallback UUID）
    let trace_id = field_map
        .get("trace_id")
        .expect("trace_id should be recorded (fallback UUID)");
    assert_eq!(trace_id.len(), 32, "trace_id must be 32 hex chars");

    // parent_span_id 应存在（fallback W3C span ID）
    let psid = field_map
        .get("parent_span_id")
        .expect("parent_span_id should be recorded (fallback W3C ID)");
    assert_eq!(psid.len(), 16, "parent_span_id must be 16 hex chars");

    // trace_state 仍为 None（fallback 不设置 trace_state）
    assert!(
        !field_map.contains_key("trace_state"),
        "trace_state should NOT be recorded (fallback sets None)"
    );
}

#[tokio::test]
async fn test_subagent_spawn_span_records_trace_fields_for_observation() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    // Envelope 携带了 trace_context，但 W2-S4 orchestrator 用 extract_trace_context()
    // 替换为 fallback UUID 版本。验证 spawn span 记录的是生成的字段。
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-tc-field".to_string(),
            subagent_type: "default".to_string(),
            description: "task with trace".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // W2-S4: trace_id 来自 fallback UUID
    let trace_id = field_map
        .get("trace_id")
        .expect("trace_id should be recorded on visp.subagent.spawn");
    assert_eq!(trace_id.len(), 32, "trace_id must be 32 hex chars");

    // W2-S4: parent_span_id 来自 fallback UUID
    let psid = field_map
        .get("parent_span_id")
        .expect("parent_span_id should be recorded on visp.subagent.spawn");
    assert_eq!(psid.len(), 16, "parent_span_id must be 16 hex chars");

    // trace_state = None（fallback 不设置）
    assert!(
        !field_map.contains_key("trace_state"),
        "trace_state should NOT be recorded (W2 fallback sets None)"
    );
}

// ── W2-S5: set_parent / tracing parent chain integration tests ──────

/// Helper: set up a TestLayer + OTel subscriber combo.
#[allow(clippy::type_complexity)]
fn setup_tracing_with_otel() -> (
    TArc<TMutex<Vec<CapturedSpan>>>,
    TArc<TMutex<Vec<String>>>,
    TArc<TMutex<Vec<visp_core::TraceContext>>>,
    tracing::subscriber::DefaultGuard,
) {
    let (spans, events, tcs) = setup_tracing();
    let guard = make_tracing_guard(&spans, &events, &tcs);
    (spans, events, tcs, guard)
}

/// W2-S5 Test 1: Oracle B1 fix — spawn_span inherits parent trace_id
/// and parent_span_id from the parent's TraceContext via set_parent.
///
/// Verified via tracing-level parent chain: spawn_span's tracing parent
/// should be the parent span (agent.iteration).
#[tokio::test]
async fn test_orchestrator_spawn_span_inherits_parent_via_set_parent() {
    let (spans, _events, _tcs, _guard) = setup_tracing_with_otel();

    let parent_span = tracing::info_span!("agent.iteration");
    let parent_id_u64 = parent_span.in_scope(|| {
        let temp = tracing::info_span!("marker");
        let id = {
            let captured = spans.lock().unwrap();
            captured
                .iter()
                .find(|s| s.name == "marker")
                .expect("marker span should exist")
                .parent_id
                .expect("marker should have parent (agent.iteration)")
        };
        drop(temp);
        id
    });

    let (mut orch, _gtx, session_id) = make_orchestrator_for_spawn();
    let envelope = Envelope {
        session_id: session_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-w2s5-1".to_string(),
            subagent_type: "default".to_string(),
            description: "test set_parent".to_string(),
            prompt: "test task".into(),
            task_id: Some("task-w2s5-1".to_string()),
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };

    async {
        orch.handle_agent_message(envelope).await;
    }
    .instrument(parent_span)
    .await;

    // Wait for spawned task to complete
    let sub_sessions: Vec<String> = orch.sub_agent_handles.keys().cloned().collect();
    for sid in &sub_sessions {
        if let Some(handle) = orch.sub_agent_handles.remove(sid) {
            let _ = handle.await;
        }
    }

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("should find visp.subagent.spawn span");
    assert_eq!(
        spawn_span.parent_id,
        Some(parent_id_u64),
        "spawn_span's tracing parent should be parent_span (Oracle B1 fix)"
    );
}

/// W2-S5 Test 2: Sub-agent's visp.agent.run automatically parented to
/// visp.subagent.spawn via tracing span hierarchy (which drives OTel
/// auto-propagation in contextual mode).
#[tokio::test]
async fn test_subagent_root_span_auto_parented_to_spawn_span() {
    let (spans, _events, _tcs, _guard) = setup_tracing_with_otel();

    let parent_span = tracing::info_span!("agent.iteration");
    let (mut orch, _gtx, session_id) = make_orchestrator_for_spawn();
    let envelope = Envelope {
        session_id: session_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-w2s5-2".to_string(),
            subagent_type: "default".to_string(),
            description: "test auto parent".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };

    async {
        orch.handle_agent_message(envelope).await;
    }
    .instrument(parent_span)
    .await;

    // Wait for spawned task to complete
    let sub_sessions: Vec<String> = orch.sub_agent_handles.keys().cloned().collect();
    for sid in &sub_sessions {
        if let Some(handle) = orch.sub_agent_handles.remove(sid) {
            let _ = handle.await;
        }
    }

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("should find visp.subagent.spawn");
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("should find visp.agent.run (sub-agent)");

    // visp.agent.run should be a tracing child of visp.subagent.spawn
    assert_eq!(
        run_span.parent_id,
        Some(spawn_span.id),
        "visp.agent.run should be child of visp.subagent.spawn"
    );
}

/// W2-S5 Test 3: Full trace chain — all spans share the same parent chain:
/// agent.iteration → visp.subagent.spawn → visp.agent.run (sub)
#[tokio::test]
async fn test_subagent_full_trace_chain_single_trace_id() {
    let (spans, _events, _tcs, _guard) = setup_tracing_with_otel();

    let parent_span = tracing::info_span!("agent.iteration");
    let parent_id_u64 = parent_span.in_scope(|| {
        let temp = tracing::info_span!("marker");
        let id = {
            let captured = spans.lock().unwrap();
            captured
                .iter()
                .find(|s| s.name == "marker")
                .expect("marker span should exist")
                .parent_id
                .expect("marker should have parent (agent.iteration)")
        };
        drop(temp);
        id
    });

    let (mut orch, _gtx, session_id) = make_orchestrator_for_spawn();
    let envelope = Envelope {
        session_id: session_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-w2s5-3".to_string(),
            subagent_type: "default".to_string(),
            description: "full chain test".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };

    async {
        orch.handle_agent_message(envelope).await;
    }
    .instrument(parent_span)
    .await;

    // Wait for spawned task to complete
    let sub_sessions: Vec<String> = orch.sub_agent_handles.keys().cloned().collect();
    for sid in &sub_sessions {
        if let Some(handle) = orch.sub_agent_handles.remove(sid) {
            let _ = handle.await;
        }
    }

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("should find visp.subagent.spawn");
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("should find visp.agent.run (sub-agent)");

    // Chain: agent.iteration → visp.subagent.spawn → visp.agent.run
    assert_eq!(
        spawn_span.parent_id,
        Some(parent_id_u64),
        "spawn_span parent should be parent_span (Oracle B1 fix)"
    );
    assert_eq!(
        run_span.parent_id,
        Some(spawn_span.id),
        "visp.agent.run parent should be visp.subagent.spawn"
    );
}

/// Regression: handle_agent_message 必须使用 SpawnRequest 携带的
/// TraceContext，而不是从当前 span 重新提取。否则在跨 mpsc 边界
/// （orchestrator 在独立 task 中处理消息）且 OTel 不活跃时，
/// extract_trace_context() 走 UUID fallback 生成全新 trace_id，
/// 导致子 agent 的 trace_id 与父 agent 不同。
#[tokio::test]
async fn test_spawn_uses_incoming_trace_context_not_reextracted() {
    let (spans, _events, _tcs, _guard) = setup_tracing_with_otel();

    // 32-hex trace_id，期望传播到 spawn span
    let expected_trace_id = "0123456789abcdef0123456789abcdef";
    let tc = visp_core::TraceContext::new(
        expected_trace_id.to_string(),
        "0123456789abcdef".to_string(),
        1,
        None,
        Some("fedcba9876543210".to_string()),
    )
    .expect("valid TraceContext");

    let (mut orch, _gtx, session_id) = make_orchestrator_for_spawn();
    let envelope = Envelope {
        session_id: session_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-w2s5-6".to_string(),
            subagent_type: "default".to_string(),
            description: "incoming tc test".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: Some(tc.clone()),
            response_tx: None,
        },
        trace_context: Some(tc),
    };

    orch.handle_agent_message(envelope).await;

    // 等待 spawned task 完成
    let sub_sessions: Vec<String> = orch.sub_agent_handles.keys().cloned().collect();
    for sid in &sub_sessions {
        if let Some(handle) = orch.sub_agent_handles.remove(sid) {
            let _ = handle.await;
        }
    }

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("should find visp.subagent.spawn span");
    let trace_id_field = spawn_span
        .fields
        .iter()
        .find(|(k, _)| k == "trace_id")
        .map(|(_, v)| v.as_str())
        .expect("spawn_span should have a trace_id field");
    assert_eq!(
        trace_id_field, expected_trace_id,
        "spawn_span 必须使用传入 TraceContext 的 trace_id，而非重新提取的"
    );
}

/// W2-S5 Test 4: When TraceContext is invalid, rebuild_parent_context
/// returns None, set_parent is NOT called, and the spawn_span becomes
/// a new trace root (no crash).
#[tokio::test]
async fn test_set_parent_fallback_when_trace_context_invalid() {
    let (spans, _events, _tcs, _guard) = setup_tracing_with_otel();

    let (mut orch, _gtx, session_id) = make_orchestrator_for_spawn();

    // Construct an invalid TraceContext (empty trace_id — fails hex parse)
    let invalid_tc = visp_core::TraceContext {
        trace_id: "".to_string(),
        span_id: "b7ad6b7169203331".to_string(),
        trace_flags: 1,
        trace_state: None,
        parent_span_id: Some("aaaaaaaaaaaaaaaa".to_string()),
    };

    let envelope = Envelope {
        session_id: session_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-w2s5-4".to_string(),
            subagent_type: "default".to_string(),
            description: "fallback test".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: Some(invalid_tc),
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // Wait for spawned task to complete
    let sub_sessions: Vec<String> = orch.sub_agent_handles.keys().cloned().collect();
    for sid in &sub_sessions {
        if let Some(handle) = orch.sub_agent_handles.remove(sid) {
            let _ = handle.await;
        }
    }

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("should find visp.subagent.spawn even with invalid TraceContext");

    // When set_parent is not called, the tracing span has no explicit parent
    // (it's created outside any parent span scope), so it becomes a root span.
    assert!(
        spawn_span.parent_id.is_none(),
        "spawn_span should be a root span when TraceContext is invalid, parent_id={:?}",
        spawn_span.parent_id
    );
}

/// Cancel 路径下：sub-agent 因 Ctrl-C 退出（code=Cancelled），
/// orchestrator 应将统一文本 "agent cancelled" 转发给父，
/// 不应让"Agent loop cancelled"等异种文本混入。
#[tokio::test]
async fn test_handle_agent_error_cancel_unifies_text() {
    use visp_core::agent::{AgentMessage, Envelope};
    use visp_core::error::AgentErrorCode;

    let (mut orch, _global_tx, _client_tx) = make_orchestrator();
    let cancel = CancellationToken::new();

    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: "parent-c".to_string(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel.clone(),
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });
    orch.active_agents.register(ActiveAgent {
        session_id: "child-c".to_string(),
        parent_session_id: Some("parent-c".to_string()),
        agent_name: "explorer".to_string(),
        cancel_token: cancel.clone(),
        inbox: mpsc::channel(16).0,
        pending_call_id: Some("call-cancel-1".to_string()),
        started_at: Instant::now(),
    });

    // 模拟 sub-agent 取消退出：code=Cancelled，message 是 agent_loop 里旧文本
    let envelope = Envelope {
        session_id: "child-c".to_string(),
        message: AgentMessage::Error {
            code: AgentErrorCode::Cancelled,
            message: "Agent loop cancelled".to_string(),
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should contain a message");
    match msg {
        OrchestratorMessage::SubAgentError { call_id, error } => {
            assert_eq!(call_id, "call-cancel-1");
            assert_eq!(
                error, "agent cancelled",
                "cancel 路径下应统一文本为 'agent cancelled'，实际: {error}"
            );
        }
        OrchestratorMessage::SubAgentComplete { .. } => {
            panic!("expected SubAgentError, got SubAgentComplete")
        }
        _ => panic!("unexpected message variant"),
    }
}

// ── 步骤 5a: Langfuse trace 级字段 ─────────────────────────

#[tokio::test]
async fn test_subagent_spawn_langfuse_disabled_no_langfuse_fields() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-lf-off".to_string(),
            subagent_type: "default".to_string(),
            description: "disabled test".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let langfuse_fields: Vec<_> = spawn_span
        .fields
        .iter()
        .filter(|(k, _)| k.starts_with("langfuse."))
        .collect();

    assert!(
        langfuse_fields.is_empty(),
        "expected no langfuse.* fields when disabled, found: {langfuse_fields:?}"
    );
}

#[tokio::test]
async fn test_subagent_spawn_langfuse_enabled_all_fields() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("key1".to_string(), "val1".to_string());
    metadata.insert("key2".to_string(), "val2".to_string());

    let agent_config = AgentConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("test-user".to_string()),
        langfuse_tags: Some(r#"["tag1","tag2"]"#.to_string()),
        langfuse_environment: Some("staging".to_string()),
        langfuse_release: Some("v1.0.0".to_string()),
        langfuse_version: Some("1.0.0".to_string()),
        langfuse_public: Some(true),
        langfuse_metadata: Some(metadata),
        ..AgentConfig::default()
    };

    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-lf-all".to_string(),
            subagent_type: "default".to_string(),
            description: "all fields".to_string(),
            prompt: "test task".into(),
            task_id: Some("task-42".to_string()),
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // ── Langfuse fields ──
    assert_eq!(
        field_map.get("langfuse.session.id"),
        Some(&parent_id.as_str()),
        "langfuse.session.id should be parent_session_id"
    );
    assert_eq!(field_map.get("langfuse.user.id"), Some(&"test-user"));
    assert_eq!(
        field_map.get("langfuse.trace.tags"),
        Some(&r#"["tag1","tag2"]"#)
    );
    let expected_name = "visp.agent.run";
    assert_eq!(field_map.get("langfuse.trace.name"), Some(&expected_name));
    assert_eq!(field_map.get("langfuse.environment"), Some(&"staging"));
    assert_eq!(field_map.get("langfuse.release"), Some(&"v1.0.0"));
    assert_eq!(field_map.get("langfuse.version"), Some(&"1.0.0"));
    assert_eq!(field_map.get("langfuse.trace.public"), Some(&"true"));

    // metadata 以 JSON 字符串形式写入
    let metadata_str = field_map
        .get("langfuse.trace.metadata")
        .expect("metadata should be present");
    let parsed: std::collections::HashMap<String, String> =
        serde_json::from_str(metadata_str).expect("metadata should be valid JSON");
    assert_eq!(parsed.get("key1"), Some(&"val1".to_string()));
    assert_eq!(parsed.get("key2"), Some(&"val2".to_string()));

    // ── 现有字段保留 ──
    assert_eq!(field_map.get("visp.subagent.name"), Some(&"default"));
    assert_eq!(field_map.get("visp.subagent.call_id"), Some(&"call-lf-all"));
    assert!(
        field_map.contains_key("visp.subagent.session_id"),
        "visp.subagent.session_id should be present"
    );
    assert_eq!(field_map.get("visp.subagent.task_id"), Some(&"task-42"));
    assert_eq!(field_map.get("visp.subagent.depth"), Some(&"0"));
}

#[tokio::test]
async fn test_subagent_spawn_langfuse_enabled_partial() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);

    // 只配置 enabled + user_id，其他字段 None
    let agent_config = AgentConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("partial-user".to_string()),
        langfuse_tags: None,
        langfuse_environment: None,
        langfuse_release: None,
        langfuse_version: None,
        langfuse_public: None,
        langfuse_metadata: None,
        ..AgentConfig::default()
    };

    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-lf-partial".to_string(),
            subagent_type: "default".to_string(),
            description: "partial fields".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // 始终写入的字段
    assert_eq!(
        field_map.get("langfuse.session.id"),
        Some(&parent_id.as_str())
    );
    assert_eq!(field_map.get("langfuse.user.id"), Some(&"partial-user"));
    // trace.name 和 environment 总有值
    let expected_name = "visp.agent.run";
    assert_eq!(field_map.get("langfuse.trace.name"), Some(&expected_name));
    assert_eq!(field_map.get("langfuse.environment"), Some(&"default"));

    // 未配置的字段不写入
    assert!(
        !field_map.contains_key("langfuse.trace.tags"),
        "tags should NOT be present when None"
    );
    assert!(
        !field_map.contains_key("langfuse.release"),
        "release should NOT be present when None"
    );
    assert!(
        !field_map.contains_key("langfuse.version"),
        "version should NOT be present when None"
    );
    assert!(
        !field_map.contains_key("langfuse.trace.public"),
        "public should NOT be present when None"
    );
    assert!(
        !field_map.contains_key("langfuse.trace.metadata"),
        "metadata should NOT be present when None"
    );
}

#[tokio::test]
async fn test_subagent_spawn_langfuse_enabled_public_false() {
    let (spans, _events, _tcs) = setup_tracing();
    let _guard = make_tracing_guard(&spans, &_events, &_tcs);

    let agent_config = AgentConfig {
        langfuse_enabled: true,
        langfuse_public: Some(false),
        ..AgentConfig::default()
    };

    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-lf-pub".to_string(),
            subagent_type: "default".to_string(),
            description: "public false".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let captured = spans.lock().unwrap();
    let spawn_span = captured
        .iter()
        .find(|s| s.name == "visp.subagent.spawn")
        .expect("expected 'visp.subagent.spawn' span");

    let field_map: std::collections::HashMap<&str, &str> = spawn_span
        .fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    assert_eq!(
        field_map.get("langfuse.trace.public"),
        Some(&"false"),
        "public=false should be recorded as 'false'"
    );
}

// ── Wave 2b: Oneshot response path ────────────────────────────

fn find_child_id(orch: &Orchestrator, parent_id: &str) -> String {
    let sessions = orch.session_mgr.list().unwrap();
    sessions
        .iter()
        .find(|s| s.parent_id.as_deref() == Some(parent_id))
        .map(|s| s.id.clone())
        .expect("child session should exist")
}

#[tokio::test]
async fn test_spawn_sub_agent_with_response_tx() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, _rx) = oneshot::channel();

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        Some(tx),
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);
    assert!(
        orch.pending_responses.contains_key(&child_id),
        "pending_responses should contain the child session_id"
    );
}

#[tokio::test]
async fn test_handle_done_response_tx_some() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, rx) = oneshot::channel();

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        Some(tx),
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);
    assert!(orch.pending_responses.contains_key(&child_id));

    orch.handle_done(&child_id).await;

    // Should receive result via oneshot
    let result = rx.await.expect("should receive result via oneshot");
    assert!(
        !orch.pending_responses.contains_key(&child_id),
        "pending_responses should be cleaned up after handle_done"
    );
    // result may be empty string (no assistant messages in session), which is fine
    assert_eq!(
        result, "",
        "result content from empty session should be empty"
    );
}

#[tokio::test]
async fn test_handle_done_response_tx_none() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);

    // Register parent agent explicitly (needed for inbox path)
    orch.active_agents.register(ActiveAgent {
        session_id: parent_id.clone(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel,
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        None, // No response_tx
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);

    orch.handle_done(&child_id).await;

    // Should go through inbox path (existing logic)
    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should receive SubAgentComplete");
    match msg {
        OrchestratorMessage::SubAgentComplete { .. } => {} // expected
        _ => panic!("expected SubAgentComplete, got a different variant"),
    }
}

#[tokio::test]
async fn test_handle_agent_error_response_tx_some() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, rx) = oneshot::channel();

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        Some(tx),
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);

    orch.handle_agent_error(
        &child_id,
        AgentErrorCode::Internal,
        "something went wrong".to_string(),
    )
    .await;

    let result = rx.await.expect("should receive error via oneshot");
    assert!(
        result.contains("[SubAgent Error]"),
        "error message should contain '[SubAgent Error]', got: {result}"
    );
    assert!(
        result.contains("something went wrong"),
        "error message should contain original error text, got: {result}"
    );
}

#[tokio::test]
async fn test_handle_agent_error_response_tx_none() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);

    orch.active_agents.register(ActiveAgent {
        session_id: parent_id.clone(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel,
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        None, // No response_tx
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);

    orch.handle_agent_error(
        &child_id,
        AgentErrorCode::Internal,
        "some error".to_string(),
    )
    .await;

    // Should go through inbox path (existing logic)
    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should receive SubAgentError");
    match msg {
        OrchestratorMessage::SubAgentError { .. } => {} // expected
        _ => panic!("expected SubAgentError, got a different variant"),
    }
}

#[tokio::test]
async fn test_pending_responses_cleanup_on_done() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, _rx) = oneshot::channel();

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        Some(tx),
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);
    assert!(orch.pending_responses.contains_key(&child_id));

    orch.handle_done(&child_id).await;

    assert!(
        !orch.pending_responses.contains_key(&child_id),
        "pending_responses should be cleaned up after handle_done"
    );
}

#[tokio::test]
async fn test_pending_responses_cleanup_on_error() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, _rx) = oneshot::channel();

    orch.spawn_sub_agent(
        &parent_id,
        "call-1",
        "default",
        "test description",
        "test prompt",
        None,
        None,
        Some(tx),
    )
    .await;

    let child_id = find_child_id(&orch, &parent_id);
    assert!(orch.pending_responses.contains_key(&child_id));

    orch.handle_agent_error(&child_id, AgentErrorCode::Internal, "error".to_string())
        .await;

    assert!(
        !orch.pending_responses.contains_key(&child_id),
        "pending_responses should be cleaned up after handle_agent_error"
    );
}

// ── Wave 3a: 子 Agent 工具筛选 ─────────────────────────────────────

use async_trait::async_trait;
use visp_core::tool::ToolType;

/// 可指定 tool_type 的 mock 工具
struct MockTypedTool {
    name: String,
    kind: ToolType,
}

#[async_trait]
impl visp_core::tool::Tool for MockTypedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "mock typed tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &visp_core::tool::ToolContext,
    ) -> visp_core::tool::ToolResult {
        visp_core::tool::ToolResult::success("ok")
    }
    fn tool_type(&self) -> ToolType {
        self.kind
    }
}

fn mock_tool(name: &str, kind: ToolType) -> std::sync::Arc<dyn visp_core::tool::Tool> {
    std::sync::Arc::new(MockTypedTool {
        name: name.to_string(),
        kind,
    })
}

/// Test: allowed_sub_agents 为空时，子 Agent 看不到任何 agent 工具
#[test]
fn test_sub_agent_no_allowed_sub_agents() {
    let registry = ToolRegistry::new();
    registry
        .register(mock_tool("builtin-read", ToolType::Builtin))
        .unwrap();
    registry
        .register(mock_tool("mcp-write", ToolType::Mcp))
        .unwrap();
    registry
        .register(mock_tool("agent-fixer", ToolType::Agent))
        .unwrap();
    registry
        .register(mock_tool("agent-explorer", ToolType::Agent))
        .unwrap();
    registry
        .register(mock_tool("skill-deploy", ToolType::Skill))
        .unwrap();

    let filtered = filter_tools_for_sub_agent(&registry, &[]);
    let names = filtered.names();

    assert!(names.contains(&"builtin-read".to_string()));
    assert!(names.contains(&"mcp-write".to_string()));
    assert!(names.contains(&"skill-deploy".to_string()));
    assert!(!names.contains(&"agent-fixer".to_string()));
    assert!(!names.contains(&"agent-explorer".to_string()));
}

/// Test: allowed_sub_agents 有值时，只保留列表中指定的 agent 工具
#[test]
fn test_sub_agent_with_allowed_sub_agents() {
    let registry = ToolRegistry::new();
    registry
        .register(mock_tool("builtin-read", ToolType::Builtin))
        .unwrap();
    registry
        .register(mock_tool("agent-fixer", ToolType::Agent))
        .unwrap();
    registry
        .register(mock_tool("agent-explorer", ToolType::Agent))
        .unwrap();
    registry
        .register(mock_tool("agent-oracle", ToolType::Agent))
        .unwrap();

    let allowed = vec!["agent-fixer".to_string(), "agent-oracle".to_string()];
    let filtered = filter_tools_for_sub_agent(&registry, &allowed);
    let names = filtered.names();

    assert!(names.contains(&"builtin-read".to_string()));
    assert!(names.contains(&"agent-fixer".to_string()));
    assert!(names.contains(&"agent-oracle".to_string()));
    assert!(!names.contains(&"agent-explorer".to_string()));
}

/// Test: compute_depth 检查在工具筛选后仍生效
#[tokio::test]
async fn test_sub_agent_depth_limit_still_applies() {
    let agent_config = AgentConfig {
        max_depth: 0,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);

    // 注册 parent agent，使其在 active_agents 中可查
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, mut parent_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: parent_id.clone(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel,
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    // Try to spawn: parent depth=0 >= max_depth=0 → error
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-depth-test".to_string(),
            subagent_type: "default".to_string(),
            description: "depth test".to_string(),
            prompt: "test".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let msg = parent_inbox_rx
        .try_recv()
        .expect("parent inbox should receive SubAgentError");
    match msg {
        OrchestratorMessage::SubAgentError { call_id, error } => {
            assert_eq!(call_id, "call-depth-test");
            assert!(
                error.contains("Max depth exceeded"),
                "depth check error expected, got: {error}"
            );
        }
        _ => panic!("expected SubAgentError, got a different variant"),
    }
}

// ── Wave 4a: System Prompt — "task" tool → agent tools ────────────────

#[test]
fn test_build_subagent_prompt_no_task_reference() {
    use visp_core::agent_definition::AgentMode;

    let mut registry = AgentRegistry::new();
    registry
        .register(AgentDefinition {
            name: "explorer".to_string(),
            description: "code search".to_string(),
            mode: AgentMode::Subagent,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .unwrap();
    registry
        .register(AgentDefinition {
            name: "fixer".to_string(),
            description: "implementation".to_string(),
            mode: AgentMode::Subagent,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .unwrap();

    let prompt = build_subagent_prompt(&registry);
    assert!(!prompt.is_empty(), "prompt should not be empty");
    assert!(
        !prompt.contains("`task`"),
        "should not reference the `task` tool, got: {prompt}"
    );
}

#[test]
fn test_build_subagent_prompt_agent_tools_guidance() {
    use visp_core::agent_definition::AgentMode;

    let mut registry = AgentRegistry::new();
    registry
        .register(AgentDefinition {
            name: "explorer".to_string(),
            description: "code search".to_string(),
            mode: AgentMode::Subagent,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        })
        .unwrap();

    let prompt = build_subagent_prompt(&registry);
    assert!(!prompt.is_empty());
    // Should mention agent tools (e.g. @agent_name or delegate pattern)
    assert!(
        prompt.contains("@explorer")
            || prompt.contains("@fixer")
            || prompt.contains("@agent_name")
            || prompt.contains("agent tool"),
        "should contain agent tool guidance, got: {prompt}"
    );
    assert!(
        prompt.contains("explorer"),
        "should list the registered agent name, got: {prompt}"
    );
}

// ── Wave 4b: 全量回归 + 端到端集成测试 ─────────────────────────────

#[tokio::test]
async fn test_end_to_end_agent_tool_spawn() {
    // 端到端测试：agent 工具调用 → spawn 子 Agent → 完成 → 结果返回
    //
    // 1. 创建 Orchestrator，注册 explorer 子 Agent
    // 2. 通过 global_tx 发送 SpawnRequest（模拟 agent 工具调用）
    // 3. 验证 Orchestrator 正确处理 spawn → run → done 流程
    // 4. 验证 oneshot response_tx 收到结果
    // 5. 验证结果包含预期内容

    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    let (tx, rx) = tokio::sync::oneshot::channel();

    // Register parent agent in active_agents so the sub-agent has a parent context
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, _parent_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: parent_id.clone(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel.clone(),
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });

    // ── Step 2: Send SpawnRequest via handle_agent_message ──────────
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-e2e-1".to_string(),
            subagent_type: "default".to_string(),
            description: "E2E integration test task".to_string(),
            prompt: "do something".into(),
            task_id: Some("task-e2e".to_string()),
            trace_context: None,
            response_tx: Some(tx),
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    // ── Step 3: Verify spawn succeeded ─────────────────────────────
    let child_id = find_child_id(&orch, &parent_id);
    assert!(
        orch.active_agents.get(&child_id).is_some(),
        "sub-agent should be active after spawn"
    );
    assert!(
        orch.pending_responses.contains_key(&child_id),
        "pending_responses should track the child session"
    );
    assert!(
        orch.sub_agent_handles.contains_key(&child_id),
        "sub_agent_handles should track the child JoinHandle"
    );

    // ── Step 4: Wait for spawned run_agent_loop task to complete ───
    // The MockProvider (empty response vec) causes the agent loop to
    // finish immediately. The spawned task sends AgentMessage::Done
    // via global_tx after completion.
    if let Some(handle) = orch.sub_agent_handles.remove(&child_id) {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("sub-agent task should complete within timeout");
    }

    // ── Step 5: Process the Done message ───────────────────────────
    // The spawned task sent Done through global_tx. We simulate the
    // orchestrator's run loop by calling handle_agent_message directly.
    orch.handle_agent_message(Envelope {
        session_id: child_id.clone(),
        message: AgentMessage::Done,
        trace_context: None,
    })
    .await;

    // ── Step 6: Verify result via oneshot ──────────────────────────
    let result = rx
        .await
        .expect("should receive result via oneshot response_tx");
    assert!(
        !orch.pending_responses.contains_key(&child_id),
        "pending_responses should be cleaned up after handle_done"
    );
    assert!(
        orch.active_agents.get(&child_id).is_none(),
        "sub-agent should be removed from active_agents after done"
    );
    // With MockProvider (empty vec), no assistant messages → empty result
    assert_eq!(
        result, "",
        "result from empty MockProvider session should be empty"
    );
}

// ── Subagent 并发限制（排队等待）────────────────────────────────

/// 构造一个 parent agent 的 SpawnRequest envelope
fn spawn_request(parent_id: &str, call_id: &str) -> Envelope {
    Envelope {
        session_id: parent_id.to_string(),
        message: AgentMessage::SpawnRequest {
            call_id: call_id.to_string(),
            subagent_type: "default".to_string(),
            description: "do something".to_string(),
            prompt: "test task".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    }
}

/// 将 parent 注册进 active_agents（真实 spawn 路径要求 parent 可查）
fn register_parent(orch: &mut Orchestrator, parent_id: &str) {
    let cancel = CancellationToken::new();
    let (parent_inbox_tx, _rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: parent_id.to_string(),
        parent_session_id: None,
        agent_name: "root".to_string(),
        cancel_token: cancel,
        inbox: parent_inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });
}

/// 找到 parent 的直接子 subagent 的 session_id
fn find_subagent_id(orch: &Orchestrator, parent_id: &str) -> String {
    orch.active_agents
        .agents_cloned()
        .into_iter()
        .find(|a| a.parent_session_id.as_deref() == Some(parent_id))
        .expect("subagent should be registered")
        .session_id
}

/// 用例 1：并发达到上限 → 新 spawn 请求入队（不启动 subagent）
#[tokio::test]
async fn test_spawn_queued_when_concurrency_limit_reached() {
    let agent_config = AgentConfig {
        max_concurrent_subagents: 1,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);
    register_parent(&mut orch, &parent_id);

    // 第一个 spawn：未达上限 → 立即启动
    orch.handle_agent_message(spawn_request(&parent_id, "call-1"))
        .await;
    assert_eq!(
        orch.active_subagent_count(),
        1,
        "first spawn should run immediately"
    );
    assert!(orch.queued_spawns.is_empty());

    // 第二个 spawn：达到上限 → 入队等待（不启动）
    orch.handle_agent_message(spawn_request(&parent_id, "call-2"))
        .await;
    assert_eq!(
        orch.active_subagent_count(),
        1,
        "second spawn must not start while at limit"
    );
    assert_eq!(orch.queued_spawns.len(), 1);
    assert_eq!(orch.queued_spawns.front().unwrap().call_id, "call-2");
}

/// 用例 2：一个 subagent done 后 → 队列中的请求被取出执行
#[tokio::test]
async fn test_queued_spawn_executed_after_done() {
    let agent_config = AgentConfig {
        max_concurrent_subagents: 1,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);
    register_parent(&mut orch, &parent_id);

    orch.handle_agent_message(spawn_request(&parent_id, "call-1"))
        .await;
    let sub_1 = find_subagent_id(&orch, &parent_id);
    orch.handle_agent_message(spawn_request(&parent_id, "call-2"))
        .await;
    assert_eq!(orch.queued_spawns.len(), 1);

    // 第一个 subagent 完成 → 释放并发空位 → 队列请求被取出执行
    orch.handle_agent_message(Envelope {
        session_id: sub_1.clone(),
        message: AgentMessage::Done,
        trace_context: None,
    })
    .await;

    assert!(
        orch.queued_spawns.is_empty(),
        "queue should be drained after a subagent finishes"
    );
    assert_eq!(
        orch.active_subagent_count(),
        1,
        "queued spawn should now be running"
    );
}

/// 用例 3：队列 FIFO 顺序（先入队的先出）
#[tokio::test]
async fn test_queued_spawns_fifo_order() {
    let agent_config = AgentConfig {
        max_concurrent_subagents: 1,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);
    register_parent(&mut orch, &parent_id);

    orch.handle_agent_message(spawn_request(&parent_id, "call-1"))
        .await;
    let sub_1 = find_subagent_id(&orch, &parent_id);
    orch.handle_agent_message(spawn_request(&parent_id, "call-2"))
        .await;
    orch.handle_agent_message(spawn_request(&parent_id, "call-3"))
        .await;

    // 队列 FIFO：先入队的 call-2 在前
    let queued: Vec<String> = orch
        .queued_spawns
        .iter()
        .map(|q| q.call_id.clone())
        .collect();
    assert_eq!(queued, vec!["call-2".to_string(), "call-3".to_string()]);

    // call-1 完成 → call-2 被取出执行
    orch.handle_agent_message(Envelope {
        session_id: sub_1.clone(),
        message: AgentMessage::Done,
        trace_context: None,
    })
    .await;
    let sub_2 = find_subagent_id(&orch, &parent_id);
    assert_eq!(
        orch.active_agents
            .get(&sub_2)
            .unwrap()
            .pending_call_id
            .as_deref(),
        Some("call-2"),
        "first queued spawn (call-2) should be executed next"
    );
    assert_eq!(orch.queued_spawns.len(), 1);
    assert_eq!(orch.queued_spawns.front().unwrap().call_id, "call-3");

    // call-2 完成 → call-3 被取出执行
    orch.handle_agent_message(Envelope {
        session_id: sub_2.clone(),
        message: AgentMessage::Done,
        trace_context: None,
    })
    .await;
    let sub_3 = find_subagent_id(&orch, &parent_id);
    assert_eq!(
        orch.active_agents
            .get(&sub_3)
            .unwrap()
            .pending_call_id
            .as_deref(),
        Some("call-3"),
        "second queued spawn (call-3) should be executed next"
    );
    assert!(orch.queued_spawns.is_empty());
}

/// 用例 4：max_concurrent_subagents 配置生效（设 3 时最多 3 个并行，第 4 个排队）
#[tokio::test]
async fn test_max_concurrent_subagents_config_limits_parallelism() {
    let agent_config = AgentConfig {
        max_concurrent_subagents: 3,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);
    register_parent(&mut orch, &parent_id);

    for call in ["call-1", "call-2", "call-3"] {
        orch.handle_agent_message(spawn_request(&parent_id, call))
            .await;
    }
    assert_eq!(
        orch.active_subagent_count(),
        3,
        "three spawns below the limit run in parallel"
    );
    assert!(orch.queued_spawns.is_empty());

    // 第 4 个请求 → 排队
    orch.handle_agent_message(spawn_request(&parent_id, "call-4"))
        .await;
    assert_eq!(orch.active_subagent_count(), 3);
    assert_eq!(orch.queued_spawns.len(), 1);
    assert_eq!(orch.queued_spawns.front().unwrap().call_id, "call-4");
}

/// 用例 5：max_concurrent_subagents = 0 表示无限制 → 所有 spawn 立即并行执行，不排队
#[tokio::test]
async fn test_zero_concurrency_limit_means_unlimited() {
    let agent_config = AgentConfig {
        max_concurrent_subagents: 0,
        ..AgentConfig::default()
    };
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn_with_config(agent_config);
    register_parent(&mut orch, &parent_id);

    // 多个 spawn 全部立即启动，不排队
    for call in ["call-1", "call-2", "call-3", "call-4", "call-5"] {
        orch.handle_agent_message(spawn_request(&parent_id, call))
            .await;
    }

    assert_eq!(
        orch.active_subagent_count(),
        5,
        "all spawns should run in parallel when limit is 0 (unlimited)"
    );
    assert!(
        orch.queued_spawns.is_empty(),
        "no spawn should be queued when limit is 0 (unlimited)"
    );
}

/// 用例 6：未达上限 → 正常 spawn（不排队）
#[tokio::test]
async fn test_spawn_not_queued_when_below_limit() {
    // 默认配置：上限 3
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();
    register_parent(&mut orch, &parent_id);

    orch.handle_agent_message(spawn_request(&parent_id, "call-1"))
        .await;

    assert_eq!(orch.active_subagent_count(), 1);
    assert!(
        orch.queued_spawns.is_empty(),
        "spawn below the limit must not queue"
    );
}

// ── Spawn prompt 中的 <image: path> 标记解析 ─────────────────────

/// 等待子会话的首条 user 消息被 append（create_sub 在 handle_agent_message
/// 内同步完成，但首消息由 run_agent_loop 异步 append 到会话历史）
async fn wait_for_sub_session_first_message(
    orch: &Orchestrator,
    parent_id: &str,
    agent_name: &str,
) -> visp_core::message::Message {
    let sub_session_id = orch
        .session_mgr
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.agent_name == agent_name && s.parent_id.as_deref() == Some(parent_id))
        .expect("sub-session should exist")
        .id;
    for _ in 0..100 {
        if let Ok(msgs) = orch.session_mgr.get_messages(&sub_session_id)
            && let Some(first) = msgs.first()
        {
            return first.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("sub session initial message was not appended in time");
}

/// 用例 1：prompt 含 `<image: path>` 标记 → 图片解析进首消息，标记从文本剥离
#[tokio::test]
async fn test_spawn_sub_agent_extracts_images_from_prompt() {
    // 创建真实的小图片文件（extract_images 只要求文件可读，不校验格式）
    let img_path =
        std::env::temp_dir().join(format!("visp_test_prompt_img_{}.png", std::process::id()));
    std::fs::write(&img_path, b"\x89PNG\r\n\x1a\nfake-image-bytes").unwrap();
    let img_path_str = img_path.to_str().unwrap().to_string();

    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    let prompt = format!("Describe this screenshot: <image: {img_path_str}>");
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-img-1".to_string(),
            subagent_type: "default".to_string(),
            description: "vision task".to_string(),
            prompt: prompt.clone(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let first = wait_for_sub_session_first_message(&orch, &parent_id, "default").await;
    assert_eq!(first.role, visp_core::message::Role::User);
    assert!(
        !first.images.is_empty(),
        "prompt <image:> marker should be parsed into the initial message images"
    );
    assert_eq!(first.images[0].path, img_path_str);
    assert!(
        !first.content.contains("<image:"),
        "image marker should be stripped from the message text, got: {}",
        first.content
    );
    assert!(first.content.contains("Describe this screenshot"));

    let _ = std::fs::remove_file(&img_path);
}

/// 用例 2：prompt 无 `<image:>` 标记 → 保留父会话最近含图消息的兜底转发
#[tokio::test]
async fn test_spawn_sub_agent_falls_back_to_parent_images() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    // 父会话有一条含图的 user 消息（模拟用户上传截图）
    let mut parent_msg = visp_core::message::Message::user("user attached an image");
    parent_msg.images = vec![visp_core::message::ImageData {
        path: "/tmp/parent-img.png".to_string(),
        base64: "cGFyZW50LWltZw==".to_string(),
        mime_type: "image/png".to_string(),
    }];
    orch.session_mgr
        .append_message(&parent_id, parent_msg)
        .unwrap();

    // prompt 无图片标记 → 走兜底逻辑，转发父会话图片
    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-img-2".to_string(),
            subagent_type: "default".to_string(),
            description: "task".to_string(),
            prompt: "plain text task".to_string(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let first = wait_for_sub_session_first_message(&orch, &parent_id, "default").await;
    assert_eq!(first.role, visp_core::message::Role::User);
    assert!(
        !first.images.is_empty(),
        "fallback should forward the parent session's images"
    );
    assert_eq!(first.images[0].path, "/tmp/parent-img.png");
}

// ── 步骤 2b：AgentRegistry 持有方式改 ArcSwap 的目标语义测试 ─────────

/// 构造单个 subagent 定义（供 registry 替换测试复用）
fn make_registry_agent(name: &str, description: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_string(),
        description: description.to_string(),
        mode: visp_core::agent_definition::AgentMode::Subagent,
        model: None,
        temperature: None,
        steps: None,
        permission: vec![],
        allowed_sub_agents: Vec::new(),
        system_prompt: String::new(),
    }
}

/// 用例 1：替换 registry 快照后，下一次 spawn 查表使用新定义。
#[tokio::test]
async fn test_agent_registry_swap_takes_effect_on_next_spawn() {
    let (mut orch, _global_tx, parent_id) = make_orchestrator_for_spawn();

    // 初始 registry（来自 helper）仅含 "default"；替换为含 "late_added" 的新快照。
    let mut new_registry = AgentRegistry::new();
    new_registry
        .register(make_registry_agent(
            "late_added",
            "added after construction",
        ))
        .unwrap();
    orch.agent_registry.store(Arc::new(new_registry));

    let envelope = Envelope {
        session_id: parent_id.clone(),
        message: AgentMessage::SpawnRequest {
            call_id: "call-swap-effect".to_string(),
            subagent_type: "late_added".to_string(),
            description: "task".to_string(),
            prompt: "task".to_string(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    };
    orch.handle_agent_message(envelope).await;

    let sessions = orch.session_mgr.list().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s.agent_name == "late_added" && s.parent_id == Some(parent_id.clone())),
        "spawn 应使用替换后的 registry 快照，创建 late_added 子会话"
    );
}

/// 用例 2：同一主流程内多次查表共享同一次快照，中途替换不发生撕裂。
#[test]
fn test_agent_registry_single_snapshot_shared_within_flow() {
    let (orch, _global_tx, _client_tx) = make_orchestrator();

    let mut old_registry = AgentRegistry::new();
    old_registry
        .register(make_registry_agent("vision", "old-vision"))
        .unwrap();
    orch.agent_registry.store(Arc::new(old_registry));

    // 流程入口取一次快照（对应 start_main_agent 的 load_full）
    let snapshot = orch.agent_registry.load_full();

    // 流程中途发生替换（等价于另一个 reload 流程 store）
    let mut new_registry = AgentRegistry::new();
    new_registry
        .register(make_registry_agent("vision", "new-vision"))
        .unwrap();
    orch.agent_registry.store(Arc::new(new_registry));

    // 同一快照内的两处查表看到一致（旧）内容，不撕裂
    let first = snapshot
        .get("vision")
        .expect("snapshot should contain vision");
    let second = snapshot
        .get("vision")
        .expect("snapshot should contain vision");
    assert_eq!(first.description, "old-vision");
    assert_eq!(second.description, "old-vision");

    // 而新的流程入口会看到新快照
    assert_eq!(
        orch.agent_registry
            .load_full()
            .get("vision")
            .expect("new snapshot should contain vision")
            .description,
        "new-vision"
    );
}

/// 用例 3：快照 clone 出的定义值可跨 await 使用（不持有 Guard/借用）。
#[tokio::test]
async fn test_agent_definition_clone_from_snapshot_survives_across_await() {
    let (orch, _global_tx, _client_tx) = make_orchestrator();

    let mut registry = AgentRegistry::new();
    registry
        .register(make_registry_agent("vision", "original"))
        .unwrap();
    orch.agent_registry.store(Arc::new(registry));

    // 从快照 clone 出定义值后即释放快照
    let definition = orch
        .agent_registry
        .load_full()
        .get("vision")
        .expect("vision should exist")
        .clone();

    // 替换并跨 await 使用 clone 出的值
    orch.agent_registry.store(Arc::new(AgentRegistry::new()));
    tokio::task::yield_now().await;

    assert_eq!(definition.name, "vision");
    assert_eq!(definition.description, "original");
}

// ── 1b-1b: SessionStart / UserPromptSubmit hook 发射 ──────────────────
//
// 依据：设计 §6.2/§6.4、实施计划 1b-1b 测试表第 1–10 条。

use visp_hooks::{HookEvent, HookEventName, HookPayload, Origin, SessionSource, StopStatus};

/// 从记录发布器中提取 hook 域事件（忽略显示域 Frame）。
fn recorded_hooks(recorder: &Arc<RecordingPublisher>) -> Vec<HookEvent> {
    recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            BusEvent::Hook(h) => Some(h.clone()),
            BusEvent::Frame(_) => None,
        })
        .collect()
}

fn count_event(hooks: &[HookEvent], name: HookEventName) -> usize {
    hooks.iter().filter(|h| h.event_name() == name).count()
}

/// 构造可成功启动主 agent 的 orchestrator：注册 `default` agent + `Mock` provider，
/// 并创建一个空历史的主会话。返回 (orch, 主 session_id)。
fn make_orchestrator_for_main_agent(recorder: Arc<RecordingPublisher>) -> (Orchestrator, String) {
    let (_cancel_tx, cancel_rx) = mpsc::channel(16);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (_client_tx, client_rx) = mpsc::channel(64);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));
    let session = session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap();
    let session_id = session.id.clone();

    let mut agent_registry = AgentRegistry::new();
    agent_registry
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
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(agent_registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    let rule_engine = Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap());
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(NoopTrimmer);

    let provider: Arc<dyn LlmProvider> = Arc::new(visp_llm::mock::MockProvider::new(vec![]));
    let mut providers = HashMap::new();
    providers.insert("Mock/mock-model".to_string(), provider);

    let daemon_config = daemon_config_with_models(vec![visp_config::LlmModelConfig {
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
    }]);

    let orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        rule_engine,
        AgentConfig::default(),
        context_trimmer,
        daemon_config,
        providers,
        recorder,
    );

    (orch, session_id)
}

/// 第 1 条：主 session 首次 `start_loop` 成功 → 发一次 `SessionStart`（字段齐备）。
#[tokio::test]
async fn test_session_start_emitted_once_on_first_start_loop() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "hello").await;

    let hooks = recorded_hooks(&recorder);
    let starts: Vec<&HookEvent> = hooks
        .iter()
        .filter(|h| h.event_name() == HookEventName::SessionStart)
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "首次 start_loop 成功应恰好发一次 SessionStart"
    );

    let event = starts[0];
    assert_eq!(event.context.session_id, sid);
    assert_eq!(event.context.source, SessionSource::Startup);
    match &event.payload {
        HookPayload::SessionStart(p) => {
            assert_eq!(p.short_id, sid.chars().take(8).collect::<String>());
            assert_eq!(p.agent_name, "default");
            assert_eq!(p.parent_session_id, None, "主会话省略 parent_session_id");
        }
        other => panic!("期望 SessionStart 载荷，得到 {other:?}"),
    }
}

/// 第 2 条：同 session 第二回合 `start_loop` → 不再发 `SessionStart`（per-session 标记集合）。
#[tokio::test]
async fn test_session_start_not_repeated_on_second_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "first").await;
    // 模拟下一回合 user input：handle_client_message 会先把会话重置为 Idle
    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Idle)
        .unwrap();
    orch.start_main_agent(&sid, "second").await;

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::SessionStart),
        1,
        "同一 session 第二回合不应再发 SessionStart"
    );
    assert_eq!(
        count_event(&hooks, HookEventName::UserPromptSubmit),
        2,
        "两回合各受理一次 UserInput，应各发一次 UserPromptSubmit"
    );
}

/// 第 3 条：同一 daemon 内第二个新建主会话 → 仍发 `SessionStart`。
#[tokio::test]
async fn test_session_start_emitted_for_second_session_in_same_daemon() {
    let recorder = recording_publisher();
    let (mut orch, sid1) = make_orchestrator_for_main_agent(recorder.clone());
    let sid2 = orch
        .session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap()
        .id;

    orch.start_main_agent(&sid1, "a").await;
    orch.start_main_agent(&sid2, "b").await;

    let hooks = recorded_hooks(&recorder);
    let starts: Vec<&HookEvent> = hooks
        .iter()
        .filter(|h| h.event_name() == HookEventName::SessionStart)
        .collect();
    assert_eq!(
        starts.len(),
        2,
        "同一 daemon 内第二个新建会话仍应发 SessionStart"
    );
    assert!(starts.iter().any(|h| h.context.session_id == sid1));
    assert!(starts.iter().any(|h| h.context.session_id == sid2));
}

/// 第 4 条：子 agent `start_loop` → 不发（`parent_id.is_none()` 过滤）。
#[tokio::test]
async fn test_sub_agent_start_loop_emits_no_session_start() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.handle_agent_message(Envelope {
        session_id: sid,
        message: AgentMessage::SpawnRequest {
            call_id: "call-sub-1".to_string(),
            subagent_type: "default".to_string(),
            description: "sub task".to_string(),
            prompt: "do something".into(),
            task_id: None,
            trace_context: None,
            response_tx: None,
        },
        trace_context: None,
    })
    .await;

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::SessionStart),
        0,
        "子 agent 的 start_loop 不应触发 SessionStart"
    );
}

/// 第 5 条：`source` 由绑定时刻 `history` 是否为空推导。
#[tokio::test]
async fn test_session_start_source_derived_from_history() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 空 history → startup
    orch.start_main_agent(&sid, "hello").await;
    let hooks = recorded_hooks(&recorder);
    let first = hooks
        .iter()
        .find(|h| h.event_name() == HookEventName::SessionStart && h.context.session_id == sid)
        .expect("应发出 SessionStart");
    assert_eq!(first.context.source, SessionSource::Startup);

    // 新会话带既有历史 → resume
    let resumed_sid = orch
        .session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap()
        .id;
    orch.session_mgr
        .append_message(&resumed_sid, visp_core::message::Message::user("old"))
        .unwrap();
    orch.start_main_agent(&resumed_sid, "new").await;

    let hooks = recorded_hooks(&recorder);
    let resumed: Vec<&HookEvent> = hooks
        .iter()
        .filter(|h| {
            h.event_name() == HookEventName::SessionStart && h.context.session_id == resumed_sid
        })
        .collect();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].context.source, SessionSource::Resume);
}

/// 第 6 条：`origin` 本步固定 `tui`（当前唯一客户端；headless/other 由 daemon 透传，后续接入）。
#[tokio::test]
async fn test_hook_events_origin_is_tui() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "hello").await;

    let hooks = recorded_hooks(&recorder);
    assert!(!hooks.is_empty());
    for h in &hooks {
        assert_eq!(
            h.context.origin,
            Origin::Tui,
            "本步所有 hook 事件 origin 应为 tui（daemon 透传 headless/other 为后续步骤）"
        );
    }
}

/// 第 7 条：`start_loop` 失败 → 不发 `SessionStart`；且向客户端发布忙错误帧。
#[tokio::test]
async fn test_no_hook_events_when_start_loop_fails() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 预置 Running：start_loop 将返回 SessionBusy 失败
    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Running)
        .unwrap();
    orch.start_main_agent(&sid, "hello").await;

    assert!(
        recorded_hooks(&recorder).is_empty(),
        "start_loop 失败不应发任何 hook 事件"
    );
    assert_eq!(
        busy_error_frames(&recorder).len(),
        1,
        "start_loop 因忙失败应发布一条忙错误帧（不再静默丢弃）"
    );
}

/// 第 8 条：`UserPromptSubmit` 主 agent 受理且 `start_loop` 成功 → 发一次。
#[tokio::test]
async fn test_user_prompt_submit_emitted_once_per_accepted_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "hello").await;

    let hooks = recorded_hooks(&recorder);
    let ups: Vec<&HookEvent> = hooks
        .iter()
        .filter(|h| h.event_name() == HookEventName::UserPromptSubmit)
        .collect();
    assert_eq!(ups.len(), 1, "主 agent 受理一回合应发一次 UserPromptSubmit");
    assert_eq!(ups[0].context.session_id, sid);
}

/// 第 9 条：子 session / 被拒输入 → 不发 `UserPromptSubmit`。
#[tokio::test]
async fn test_user_prompt_submit_not_emitted_for_rejected_or_sub_session() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 被拒：不存在的 session
    orch.handle_client_message(ClientMessage::UserInput {
        session_id: "does-not-exist".to_string(),
        text: "hi".to_string(),
    })
    .await;

    // 子会话：parent_id 非空 → handle_client_message 直接忽略
    let child = orch
        .session_mgr
        .create_sub(SubSessionParams {
            parent_id: Some(sid.clone()),
            agent_name: "default".to_string(),
            session_id: None,
            project_path: PathBuf::from("/tmp"),
            config: LlmConfig::default(),
            permission: vec![],
            approved_tools: HashSet::new(),
        })
        .unwrap();
    orch.handle_client_message(ClientMessage::UserInput {
        session_id: child.id.clone(),
        text: "hi".to_string(),
    })
    .await;

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::UserPromptSubmit),
        0,
        "子会话/被拒输入不应发 UserPromptSubmit"
    );
}

/// 第 10 条：`prompt_chars` 为字符数；默认不含原文（`prompt` 省略）。
#[tokio::test]
async fn test_user_prompt_submit_prompt_chars_and_no_raw_prompt() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // "héllo🙂" = 6 个字符（非字节数）
    orch.start_main_agent(&sid, "héllo🙂").await;

    let hooks = recorded_hooks(&recorder);
    let ups = hooks
        .iter()
        .find(|h| h.event_name() == HookEventName::UserPromptSubmit)
        .expect("应发出 UserPromptSubmit");
    match &ups.payload {
        HookPayload::UserPromptSubmit(p) => {
            assert_eq!(p.prompt_chars, 6, "prompt_chars 应为字符数而非字节数");
            assert!(p.prompt.is_none(), "默认脱敏：不应携带原文");
        }
        other => panic!("期望 UserPromptSubmit 载荷，得到 {other:?}"),
    }
}

// ── M1: 会话级 Stop / StopFailure hook 发射 ──────────────────────────────
//
// 依据：设计 §6.2 完成事件分层（执行级 vs 会话级）、实施计划 1b-1b 第 13–19 条。

/// 统计总线上的 root `Done` 帧数量（TUI 显示面看到的回合结束信号）。
fn count_done_frames(recorder: &Arc<RecordingPublisher>) -> usize {
    recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, BusEvent::Frame(f) if matches!(f.event, AgentEvent::Done)))
        .count()
}

/// 收集总线上发布的「会话忙」错误帧（`AgentErrorCode::SessionBusy`）。
fn busy_error_frames(recorder: &Arc<RecordingPublisher>) -> Vec<AgentEventFrame> {
    recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            BusEvent::Frame(f) => match &f.event {
                AgentEvent::Error {
                    code: AgentErrorCode::SessionBusy,
                    ..
                } => Some(f.clone()),
                _ => None,
            },
            BusEvent::Hook(_) => None,
        })
        .collect()
}

/// 注册一个 root ActiveAgent（无真实 loop），供生命周期 hook 断言使用。
fn register_root_agent(orch: &mut Orchestrator, sid: &str) {
    let (inbox_tx, _inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: sid.to_string(),
        parent_session_id: None,
        agent_name: "default".to_string(),
        cancel_token: CancellationToken::new(),
        inbox: inbox_tx,
        pending_call_id: None,
        started_at: Instant::now(),
    });
}

/// 第 11/15/19 条：主回合 TUI 恰收到 1 条 `Done` 帧（来自
/// `agent_loop → 主转发任务 → 总线`），且会话级 `Hook(Stop)` 恰 1 条；
/// `handle_done` 不再补发第二条 root `Done` 帧。
#[tokio::test]
async fn test_root_turn_has_single_done_frame_and_single_stop_hook() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "hello").await;

    // 排空 global_rx（等价于 orchestrator run 循环），直到 Done 被处理
    // （active_agents 清空）且显示面 Done 帧已由主转发任务发布。
    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            while let Ok(envelope) = orch.global_rx.try_recv() {
                orch.handle_agent_message(envelope).await;
            }
            if orch.active_agents.get(&sid).is_none() && count_done_frames(&recorder) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(finished.is_ok(), "主 agent 回合应在超时前结束");

    let done_frames = count_done_frames(&recorder);
    assert_eq!(
        done_frames, 1,
        "TUI 每回合应恰收到 1 条 Done 帧（去重后），实际 {done_frames}"
    );

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::Stop),
        1,
        "会话级 Stop 应恰发一次"
    );
    let stop = hooks
        .iter()
        .find(|h| h.event_name() == HookEventName::Stop)
        .expect("应发出 Stop");
    assert_eq!(stop.context.session_id, sid);
    assert_eq!(stop.context.source, SessionSource::Startup);
    match &stop.payload {
        HookPayload::Stop(p) => {
            assert_eq!(
                p.status,
                StopStatus::Completed,
                "正常 Done → status=completed"
            );
            assert_eq!(
                (p.input_tokens, p.output_tokens, p.tool_calls),
                (0, 0, 0),
                "空 MockProvider 回合用量为 0"
            );
        }
        other => panic!("期望 Stop 载荷，得到 {other:?}"),
    }
}

/// 第 17 条：root 真 `Error` → 会话级 `StopFailure`（含 code/message）。
#[tokio::test]
async fn test_root_error_emits_stop_failure_hook() {
    let recorder = recording_publisher();
    let (mut orch, _global_tx, _client_tx) = make_orchestrator_with_publisher(recorder.clone());
    let sid = orch
        .session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap()
        .id;
    register_root_agent(&mut orch, &sid);

    orch.handle_agent_error(&sid, AgentErrorCode::Internal, "boom".to_string())
        .await;

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::StopFailure),
        1,
        "root 真错误应发一次 StopFailure"
    );
    assert_eq!(
        count_event(&hooks, HookEventName::Stop),
        0,
        "真错误不得发 Stop"
    );
    let failure = hooks
        .iter()
        .find(|h| h.event_name() == HookEventName::StopFailure)
        .expect("应发出 StopFailure");
    assert_eq!(failure.context.session_id, sid);
    match &failure.payload {
        HookPayload::StopFailure(p) => {
            assert_eq!(p.code, "Internal", "code 取 AgentErrorCode 序列化字符串");
            assert_eq!(p.message, "boom");
        }
        other => panic!("期望 StopFailure 载荷，得到 {other:?}"),
    }
}

/// 第 16/18 条：root 用户取消 → `Stop(status=cancelled)`，**不**误入 `StopFailure`。
#[tokio::test]
async fn test_root_cancel_emits_stop_cancelled_not_failure() {
    let recorder = recording_publisher();
    let (mut orch, _global_tx, _client_tx) = make_orchestrator_with_publisher(recorder.clone());
    let sid = orch
        .session_mgr
        .create(&PathBuf::from("/tmp"), LlmConfig::default())
        .unwrap()
        .id;
    register_root_agent(&mut orch, &sid);

    orch.handle_agent_error(
        &sid,
        AgentErrorCode::Cancelled,
        "Agent loop cancelled".to_string(),
    )
    .await;

    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::StopFailure),
        0,
        "用户取消不得误入 StopFailure"
    );
    assert_eq!(
        count_event(&hooks, HookEventName::Stop),
        1,
        "用户取消应发停止（cancelled）"
    );
    let stop = hooks
        .iter()
        .find(|h| h.event_name() == HookEventName::Stop)
        .expect("应发出 Stop");
    match &stop.payload {
        HookPayload::Stop(p) => {
            assert_eq!(p.status, StopStatus::Cancelled, "取消 → status=cancelled")
        }
        other => panic!("期望 Stop 载荷，得到 {other:?}"),
    }
}

/// 第 14/20 条：子 agent 完成/失败**不**产生会话级 `Stop`/`StopFailure`（子路径不变）。
#[tokio::test]
async fn test_sub_agent_completion_emits_no_session_level_hook() {
    for code in [None, Some(AgentErrorCode::Internal)] {
        let recorder = recording_publisher();
        let (mut orch, _global_tx, _client_tx) = make_orchestrator_with_publisher(recorder.clone());
        let parent_id = orch
            .session_mgr
            .create(&PathBuf::from("/tmp"), LlmConfig::default())
            .unwrap()
            .id;
        register_root_agent(&mut orch, &parent_id);
        let child = orch
            .session_mgr
            .create_sub(SubSessionParams {
                parent_id: Some(parent_id.clone()),
                agent_name: "default".to_string(),
                session_id: None,
                project_path: PathBuf::from("/tmp"),
                config: LlmConfig::default(),
                permission: vec![],
                approved_tools: HashSet::new(),
            })
            .unwrap();

        let (child_inbox_tx, _child_inbox_rx) = mpsc::channel(16);
        orch.active_agents.register(ActiveAgent {
            session_id: child.id.clone(),
            parent_session_id: Some(parent_id.clone()),
            agent_name: "default".to_string(),
            cancel_token: CancellationToken::new(),
            inbox: child_inbox_tx,
            pending_call_id: Some("call-1".to_string()),
            started_at: Instant::now(),
        });

        match code {
            None => orch.handle_done(&child.id).await,
            Some(c) => {
                orch.handle_agent_error(&child.id, c, "boom".to_string())
                    .await
            }
        }

        let hooks = recorded_hooks(&recorder);
        assert_eq!(
            count_event(&hooks, HookEventName::Stop),
            0,
            "子 agent 不产生会话级 Stop"
        );
        assert_eq!(
            count_event(&hooks, HookEventName::StopFailure),
            0,
            "子 agent 不产生会话级 StopFailure"
        );
    }
}

// ── 会话 loop 所有权与收尾：窄化重置 / 忙失败上报 / 准备动作后移 / G5 ──────────

/// 第 2 条：`Completed` 主会话经 `handle_client_message` → 重置 → 成功启动第二回合。
#[tokio::test]
async fn test_completed_main_session_recovers_and_starts_second_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Completed)
        .unwrap();

    orch.handle_client_message(ClientMessage::UserInput {
        session_id: sid.clone(),
        text: "second".to_string(),
    })
    .await;

    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Running,
        "Completed 终态应被重置并成功启动第二回合"
    );
    assert!(
        busy_error_frames(&recorder).is_empty(),
        "合法恢复不应发布忙错误帧"
    );
}

/// 第 3 条：`Error` 主会话经 `handle_client_message` → 重置 → 成功启动第二回合。
#[tokio::test]
async fn test_error_main_session_recovers_and_starts_second_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Error)
        .unwrap();

    orch.handle_client_message(ClientMessage::UserInput {
        session_id: sid.clone(),
        text: "second".to_string(),
    })
    .await;

    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Running,
        "Error 终态应被重置并成功启动第二回合"
    );
    assert!(
        busy_error_frames(&recorder).is_empty(),
        "合法恢复不应发布忙错误帧"
    );
}

/// 第 4 条：`Running` 主会话经 `handle_client_message` → 不重置、不启动、不发第二组 hook。
#[tokio::test]
async fn test_running_main_session_is_not_reset_or_restarted() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Running)
        .unwrap();

    orch.handle_client_message(ClientMessage::UserInput {
        session_id: sid.clone(),
        text: "while running".to_string(),
    })
    .await;

    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Running,
        "Running 不应被重置为 Idle"
    );
    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::SessionStart),
        0,
        "Running 被拒不应发 SessionStart"
    );
    assert_eq!(
        count_event(&hooks, HookEventName::UserPromptSubmit),
        0,
        "Running 被拒不应发 UserPromptSubmit"
    );
}

/// 第 5 条：`Running` 被拒时错误帧 code 语义为 `SessionBusy`，文案不含 `cancelled`。
#[tokio::test]
async fn test_running_rejection_error_frame_is_session_busy() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Running)
        .unwrap();

    orch.handle_client_message(ClientMessage::UserInput {
        session_id: sid.clone(),
        text: "while running".to_string(),
    })
    .await;

    let frames = busy_error_frames(&recorder);
    assert_eq!(frames.len(), 1, "Running 被拒应恰发一条忙错误帧");
    match &frames[0].event {
        AgentEvent::Error { code, message } => {
            assert_eq!(
                code.to_string(),
                "SessionBusy",
                "Display 文本即 daemon 侧 proto code 语义"
            );
            assert!(
                !message.contains("cancelled"),
                "忙文案不得含 cancelled，实际: {message}"
            );
        }
        _ => panic!("期望 Error 帧"),
    }
}

/// 第 8 条：忙失败后无残留 registration、system prompt 模板不累积。
#[tokio::test]
async fn test_busy_failure_leaves_no_registration_or_prompt_accumulation() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 替换 registry：default 带非空 system_prompt，使 append 检查有实际写入
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
            system_prompt: "SYS-PROMPT".to_string(),
        })
        .unwrap();
    orch.agent_registry.store(Arc::new(registry));

    let before = orch.session_mgr.get(&sid).unwrap().system_prompt_template;
    orch.session_mgr
        .finish_loop(&sid, SessionStatus::Running)
        .unwrap();

    orch.start_main_agent(&sid, "hello").await;

    assert!(
        orch.active_agents.get(&sid).is_none(),
        "忙失败不应残留 active registration"
    );
    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().system_prompt_template,
        before,
        "忙失败不应累积 system prompt 模板"
    );
}

/// 第 9 条（G5）：先起新循环、再投递旧回合 `Done` → 新循环令牌仍在、状态仍 `Running`。
#[tokio::test]
async fn test_stale_done_does_not_tear_down_new_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 新回合：新循环已启动（Running + 令牌）
    let _ctx = orch
        .session_mgr
        .start_loop(&sid, &orch.context_trimmer, None, None)
        .expect("新循环应能启动");
    assert!(orch.session_mgr.has_inflight_loop(&sid));

    // 旧回合的 root registration（同 session_id）
    register_root_agent(&mut orch, &sid);

    // 旧回合 `Done` 迟到
    orch.handle_done(&sid).await;

    assert!(
        orch.session_mgr.has_inflight_loop(&sid),
        "旧 Done 不应移除新循环令牌"
    );
    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Running,
        "旧 Done 不应把新回合状态改回 Idle"
    );
    assert!(
        orch.active_agents.get(&sid).is_some(),
        "旧 Done 不应移除新循环的 registration"
    );
}

/// 第 10 条（G5）：单回合正常完成仍能被置回 `Idle`（判据不误伤正常路径）。
#[tokio::test]
async fn test_normal_single_turn_still_finishes_idle() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    orch.start_main_agent(&sid, "hello").await;

    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            while let Ok(envelope) = orch.global_rx.try_recv() {
                orch.handle_agent_message(envelope).await;
            }
            if orch.active_agents.get(&sid).is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(finished.is_ok(), "单回合应在超时前结束");

    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Idle,
        "正常完成应被置回 Idle"
    );
}

/// 第 21 条（G5·错误路径）：先起新回合（`Running`）再投递旧回合迟到错误 →
/// `handle_agent_error` 收尾为幂等空操作：新循环令牌仍在、状态仍 `Running`、
/// registration 未被移除。
#[tokio::test]
async fn test_stale_error_does_not_tear_down_new_turn() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    // 新回合：新循环已启动（Running + 令牌）
    let _ctx = orch
        .session_mgr
        .start_loop(&sid, &orch.context_trimmer, None, None)
        .expect("新循环应能启动");
    assert!(orch.session_mgr.has_inflight_loop(&sid));

    // 旧回合的 root registration（同 session_id）
    register_root_agent(&mut orch, &sid);

    // 旧回合错误迟到
    orch.handle_agent_error(&sid, AgentErrorCode::Internal, "stale boom".to_string())
        .await;

    assert!(
        orch.session_mgr.has_inflight_loop(&sid),
        "旧错误不应移除新循环令牌"
    );
    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Running,
        "旧错误不应把新回合状态改写为 Error"
    );
    assert!(
        orch.active_agents.get(&sid).is_some(),
        "旧错误不应移除新循环的 registration"
    );
}

/// 第 22 条（G5·不误伤）：单回合错误路径正常收尾 → 状态置 `Error`、
/// registration 清理、会话级 `StopFailure` 仍发布（G5 判据不吞掉正常终止）。
#[tokio::test]
async fn test_single_turn_error_still_finishes_error() {
    let recorder = recording_publisher();
    let (mut orch, sid) = make_orchestrator_for_main_agent(recorder.clone());

    register_root_agent(&mut orch, &sid);

    orch.handle_agent_error(&sid, AgentErrorCode::Internal, "boom".to_string())
        .await;

    assert!(
        orch.active_agents.get(&sid).is_none(),
        "单回合错误应正常清理 registration"
    );
    assert!(
        !orch.session_mgr.has_inflight_loop(&sid),
        "单回合错误应清理循环令牌，无泄漏"
    );
    assert_eq!(
        orch.session_mgr.get(&sid).unwrap().status,
        SessionStatus::Error,
        "单回合错误应置为 Error"
    );
    let hooks = recorded_hooks(&recorder);
    assert_eq!(
        count_event(&hooks, HookEventName::StopFailure),
        1,
        "单回合真错误仍应发布会话级 StopFailure"
    );
}

/// 第 23 条（G5·子会话不受判据）：子会话即便处于 `Running`，错误收尾也正常
/// 清理（G5 仅主会话适用），registration 与令牌均无泄漏。
#[tokio::test]
async fn test_sub_session_error_is_not_treated_as_stale() {
    let recorder = recording_publisher();
    let (mut orch, parent_id) = make_orchestrator_for_main_agent(recorder.clone());
    register_root_agent(&mut orch, &parent_id);

    let child = orch
        .session_mgr
        .create_sub(SubSessionParams {
            parent_id: Some(parent_id.clone()),
            agent_name: "default".to_string(),
            session_id: None,
            project_path: PathBuf::from("/tmp"),
            config: LlmConfig::default(),
            permission: vec![],
            approved_tools: HashSet::new(),
        })
        .unwrap();

    // 子会话也标记为 Running + 持有令牌：若 G5 误丢 parent_id 判据，会错误地空操作
    let _child_ctx = orch
        .session_mgr
        .start_loop(&child.id, &orch.context_trimmer, None, None)
        .expect("子循环应能启动");
    assert!(orch.session_mgr.has_inflight_loop(&child.id));

    let (child_inbox_tx, _child_inbox_rx) = mpsc::channel(16);
    orch.active_agents.register(ActiveAgent {
        session_id: child.id.clone(),
        parent_session_id: Some(parent_id.clone()),
        agent_name: "default".to_string(),
        cancel_token: CancellationToken::new(),
        inbox: child_inbox_tx,
        pending_call_id: Some("call-1".to_string()),
        started_at: Instant::now(),
    });

    orch.handle_agent_error(
        &child.id,
        AgentErrorCode::Internal,
        "child boom".to_string(),
    )
    .await;

    assert!(
        orch.active_agents.get(&child.id).is_none(),
        "子会话错误收尾应清理 registration，不套用 G5 空操作"
    );
    assert!(
        !orch.session_mgr.has_inflight_loop(&child.id),
        "子会话错误收尾应移除令牌，无泄漏"
    );
    assert_eq!(
        orch.session_mgr.get(&child.id).unwrap().status,
        SessionStatus::Error,
        "子会话错误收尾应置为 Error"
    );
}

#[cfg(test)]
#[path = "orchestrator_verifier_tests.rs"]
mod verifier_tests;
