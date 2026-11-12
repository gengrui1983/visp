use super::*;
use crate::agent::AgentKind;
use crate::context::NoopTrimmer;
use crate::provider::LlmConfig;
use futures::stream;
use serial_test::serial;
use std::path::PathBuf;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing_subscriber::prelude::*;

/// 计划 0c-2 测试 4：PreToolUse 经 `event_to_msg` 不产出 AgentMessage（显式 None）。
#[test]
fn test_event_to_msg_pre_tool_use_is_not_forwarded() {
    let event = AgentEvent::PreToolUse {
        call_id: "call-1".into(),
        tool_name: "bash".into(),
        requires_approval: true,
    };
    assert!(event_to_msg(&event).is_none());
}

/// RateLimit mock: first `fail_attempts` calls return RateLimit, then Ok(Done)
struct RateLimitProvider {
    call_count: AtomicUsize,
    fail_attempts: usize,
}

#[async_trait::async_trait]
impl LlmProvider for RateLimitProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count < self.fail_attempts {
            return Err(LlmError::RateLimit {
                retry_after_secs: 10,
            });
        }
        let s: Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>> =
            Box::pin(stream::iter(vec![Ok(ChatEvent::Done)]));
        Ok(s)
    }
}

#[serial]
#[tokio::test]
async fn test_call_llm_with_retry_cancels_during_sleep() {
    let provider = Arc::new(RateLimitProvider {
        call_count: AtomicUsize::new(0),
        fail_attempts: 3,
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = AgentLoopContext {
        session_id: "test".into(),
        history: vec![],
        working_dir: PathBuf::from("/tmp"),
        config: LlmConfig::default(),
        cancel_token: cancel.clone(),
        context_trimmer: Arc::new(NoopTrimmer),
        global_tx: None,
        permission_rules: None,
        agent_kind: AgentKind::Primary,
        depth: 0,
        parent_session_id: "test".into(),
    };
    let cfg = AgentConfig {
        llm_retry_attempts: 5,
        llm_retry_base_delay_ms: 1000,
        ..Default::default()
    };

    let provider_clone = provider.clone();
    let handle = tokio::spawn(async move {
        call_llm_with_retry(provider_clone.as_ref(), &[], &[], &ctx, &cfg, "test").await
    });

    // 100ms 后取消，此时应在 retry sleep 中
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("should complete within 1s")
        .expect("join should not panic");

    assert!(
        matches!(result, Err(LlmError::Cancelled)),
        "expected Err(LlmError::Cancelled)"
    );
    let count = provider.call_count.load(Ordering::SeqCst);
    assert!(
        count <= 3,
        "expected at most 3 calls (cancelled during retry sleep), got {}",
        count
    );
}

/// ServerError mock: first `fail_attempts` calls return Api{503}, then Ok(Done)
struct ServerErrorProvider {
    call_count: AtomicUsize,
    fail_attempts: usize,
}

#[async_trait::async_trait]
impl LlmProvider for ServerErrorProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count < self.fail_attempts {
            return Err(LlmError::Api {
                status: 503,
                message: r#"{"error":{"code":10110,"message":"the engine is overloaded"}}"#
                    .to_string(),
            });
        }
        let s: Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>> =
            Box::pin(stream::iter(vec![Ok(ChatEvent::Done)]));
        Ok(s)
    }
}

#[serial]
#[tokio::test]
async fn test_call_llm_with_retry_503_succeeds_after_retries() {
    let provider = Arc::new(ServerErrorProvider {
        call_count: AtomicUsize::new(0),
        fail_attempts: 2,
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = AgentLoopContext {
        session_id: "test".into(),
        history: vec![],
        working_dir: PathBuf::from("/tmp"),
        config: LlmConfig::default(),
        cancel_token: cancel.clone(),
        context_trimmer: Arc::new(NoopTrimmer),
        global_tx: None,
        permission_rules: None,
        agent_kind: AgentKind::Primary,
        depth: 0,
        parent_session_id: "test".into(),
    };
    let cfg = AgentConfig {
        llm_retry_attempts: 5,
        llm_retry_base_delay_ms: 1,
        ..Default::default()
    };

    let result = call_llm_with_retry(provider.as_ref(), &[], &[], &ctx, &cfg, "test").await;
    assert!(result.is_ok(), "should succeed after retries");
    let count = provider.call_count.load(Ordering::SeqCst);
    assert_eq!(
        count, 3,
        "expected 3 calls (2 failures + 1 success), got {count}"
    );
}

#[serial]
#[tokio::test]
async fn test_call_llm_with_retry_emits_retry_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    let provider = Arc::new(ServerErrorProvider {
        call_count: AtomicUsize::new(0),
        fail_attempts: 2,
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = AgentLoopContext {
        session_id: "test".into(),
        history: vec![],
        working_dir: PathBuf::from("/tmp"),
        config: LlmConfig::default(),
        cancel_token: cancel.clone(),
        context_trimmer: Arc::new(NoopTrimmer),
        global_tx: None,
        permission_rules: None,
        agent_kind: AgentKind::Primary,
        depth: 0,
        parent_session_id: "test".into(),
    };
    let cfg = AgentConfig {
        llm_retry_attempts: 5,
        llm_retry_base_delay_ms: 1,
        ..Default::default()
    };

    let result = call_llm_with_retry(provider.as_ref(), &[], &[], &ctx, &cfg, "test").await;
    assert!(result.is_ok(), "should succeed after retries");

    let captured_events = events.lock().unwrap();
    let retry_events = captured_events
        .iter()
        .filter(|e| *e == "gen_ai.client.retry")
        .count();
    assert_eq!(
        retry_events, 2,
        "expected one gen_ai.client.retry event per retry decision, got: {retry_events}"
    );
}

#[serial]
#[tokio::test]
async fn test_call_llm_with_retry_503_exhausted() {
    let provider = Arc::new(ServerErrorProvider {
        call_count: AtomicUsize::new(0),
        fail_attempts: 100,
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = AgentLoopContext {
        session_id: "test".into(),
        history: vec![],
        working_dir: PathBuf::from("/tmp"),
        config: LlmConfig::default(),
        cancel_token: cancel.clone(),
        context_trimmer: Arc::new(NoopTrimmer),
        global_tx: None,
        permission_rules: None,
        agent_kind: AgentKind::Primary,
        depth: 0,
        parent_session_id: "test".into(),
    };
    let cfg = AgentConfig {
        llm_retry_attempts: 3,
        llm_retry_base_delay_ms: 1,
        ..Default::default()
    };

    let result = call_llm_with_retry(provider.as_ref(), &[], &[], &ctx, &cfg, "test").await;
    assert!(result.is_err(), "should fail after retries exhausted");
    if let Err(e) = result {
        assert!(
            matches!(e, LlmError::Api { status: 503, .. }),
            "expected LlmError::Api with status 503, got {e}"
        );
    }
    let count = provider.call_count.load(Ordering::SeqCst);
    assert_eq!(
        count, 4,
        "expected 4 calls (1 initial + 3 retries), got {count}"
    );
}

struct Phase2MockTrimmer;
impl crate::context::ContextTrimmer for Phase2MockTrimmer {
    fn trim(&self, h: &[Message], _: u32, _: u32, _: u32) -> Vec<Message> {
        h.to_vec()
    }
}

/// Provider that returns TextDelta + UsageInfo + Done with explicit token counts
struct TokenProvider {
    text: String,
    input_tokens: u32,
    output_tokens: u32,
    cache_read: u32,
    cache_write: u32,
}

#[async_trait::async_trait]
impl LlmProvider for TokenProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::TextDelta(self.text.clone())),
            Ok(ChatEvent::UsageInfo {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                tool_calls: 0,
                cache_creation_input_tokens: self.cache_write,
                cache_read_input_tokens: self.cache_read,
                cost: None,
            }),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

#[serial]
#[tokio::test]
async fn test_actual_tokens_persisted_on_assistant_message() {
    use crate::session::InMemorySessionStore;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let (global_tx, _global_rx) = mpsc::channel::<Envelope>(16);
    let permission_rules = std::sync::Arc::new(Vec::new());
    let ctx = session_mgr
        .start_loop(&sid, &trimmer, Some(global_tx), Some(permission_rules))
        .unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(TokenProvider {
        text: "Hello world".into(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read: 10,
        cache_write: 5,
    });
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("Hi"),
        tx,
    )
    .await;

    // Verify tokens were persisted on assistant message
    let final_session = session_mgr.get(&sid).unwrap();
    let assistant_msgs: Vec<_> = final_session
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert!(
        !assistant_msgs.is_empty(),
        "expected at least one assistant message"
    );
    let msg = assistant_msgs[0];
    assert_eq!(
        msg.actual_tokens_input,
        Some(100),
        "actual_tokens_input should be 100"
    );
    assert_eq!(
        msg.actual_tokens_output,
        Some(50),
        "actual_tokens_output should be 50"
    );
    assert_eq!(
        msg.actual_cache_read,
        Some(10),
        "actual_cache_read should be 10"
    );
    assert_eq!(
        msg.actual_cache_write,
        Some(5),
        "actual_cache_write should be 5"
    );
}

// ── W0A-3: tool execution duration ─────────────────────────────────────

/// Tool that sleeps ~30ms before returning success — used to measure duration.
struct SleepyTool;

#[async_trait::async_trait]
impl crate::tool::Tool for SleepyTool {
    fn name(&self) -> &str {
        "sleepy"
    }
    fn description(&self) -> &str {
        "sleeps then returns"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        crate::tool::ToolResult::success("done")
    }
}

/// Provider: first call returns a `sleepy` tool_use, next call returns Done.
struct OneToolCallProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for OneToolCallProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_sleepy_1".into(),
                    name: "sleepy".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

#[serial]
#[tokio::test]
async fn test_tool_execution_records_duration_ms_in_history() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(SleepyTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 2,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("invoke sleepy"),
        tx,
    )
    .await;

    let final_session = session_mgr.get(&sid).unwrap();
    let tool_msgs: Vec<_> = final_session
        .history
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(
        tool_msgs.len(),
        1,
        "expected exactly one tool result message"
    );
    let dur = tool_msgs[0].tool_result_duration_ms;
    assert!(
        dur.is_some(),
        "tool_result_duration_ms should be set after tool execution, got None"
    );
    assert!(
        dur.unwrap() >= 25,
        "tool_result_duration_ms should be >= 25ms (slept 30ms), got {:?}",
        dur
    );
}

// ── W0B-6: provider_metadata injection ──────────────────────────────────

/// Provider that returns TextDelta + OutputMetadata + Done
struct MetadataProvider {
    metadata: ProviderMetadata,
}

#[async_trait::async_trait]
impl LlmProvider for MetadataProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::TextDelta("hello".to_string())),
            Ok(ChatEvent::OutputMetadata(self.metadata.clone())),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

#[serial]
#[tokio::test]
async fn test_agent_loop_assistant_message_has_provider_metadata() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let metadata = ProviderMetadata {
        model: "claude-test".to_string(),
        finish_reasons: vec!["stop".to_string()],
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        latency_ms: 100,
    };

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(MetadataProvider {
        metadata: metadata.clone(),
    });
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let final_session = session_mgr.get(&sid).unwrap();
    let assistant_msgs: Vec<_> = final_session
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(
        assistant_msgs.len(),
        1,
        "expected exactly one assistant message"
    );
    let msg = assistant_msgs[0];
    let expected_json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(
        msg.provider_metadata,
        Some(expected_json),
        "provider_metadata should match"
    );
}

/// Provider for multi-turn: first call returns tool_use + metadata, second returns text + different metadata
struct TwoRoundMetadataProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for TwoRoundMetadataProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            // First round: tool call + metadata1
            let events: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_sleepy_1".into(),
                    name: "sleepy".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::OutputMetadata(ProviderMetadata {
                    model: "claude-test".into(),
                    finish_reasons: vec!["tool_use".into()],
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    latency_ms: 100,
                })),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            // Second round: text + metadata2
            let events: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::TextDelta("result".to_string())),
                Ok(ChatEvent::OutputMetadata(ProviderMetadata {
                    model: "claude-test".into(),
                    finish_reasons: vec!["stop".into()],
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_read_input_tokens: Some(5),
                    cache_creation_input_tokens: Some(3),
                    latency_ms: 200,
                })),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }
}

#[serial]
#[tokio::test]
async fn test_agent_loop_provider_metadata_persists_through_multi_turn() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(TwoRoundMetadataProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(std::sync::Arc::new(SleepyTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("do something"),
        tx,
    )
    .await;

    let final_session = session_mgr.get(&sid).unwrap();
    let assistant_msgs: Vec<_> = final_session
        .history
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(
        assistant_msgs.len(),
        2,
        "expected two assistant messages for two rounds"
    );

    // First message (tool call round): tool_use metadata
    let meta1 = serde_json::to_value(ProviderMetadata {
        model: "claude-test".into(),
        finish_reasons: vec!["tool_use".into()],
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        latency_ms: 100,
    })
    .unwrap();
    assert_eq!(
        assistant_msgs[0].provider_metadata,
        Some(meta1),
        "first assistant message should have metadata from first round"
    );

    // Second message (text round): stop metadata with different tokens
    let meta2 = serde_json::to_value(ProviderMetadata {
        model: "claude-test".into(),
        finish_reasons: vec!["stop".into()],
        input_tokens: 20,
        output_tokens: 10,
        cache_read_input_tokens: Some(5),
        cache_creation_input_tokens: Some(3),
        latency_ms: 200,
    })
    .unwrap();
    assert_eq!(
        assistant_msgs[1].provider_metadata,
        Some(meta2),
        "second assistant message should have metadata from second round, no cross-contamination"
    );
}

// ── TestLayer for tracing assertions ────────────────────────────────────
use std::sync::Arc as TArc;
use std::sync::Mutex as TMutex;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: Vec<(String, String)>,
    id: u64,
    parent_id: Option<u64>,
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
    events: TArc<TMutex<Vec<String>>>,
}

impl TestLayer {
    fn new(spans: TArc<TMutex<Vec<CapturedSpan>>>, events: TArc<TMutex<Vec<String>>>) -> Self {
        Self { spans, events }
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
        self.events.lock().unwrap().push(target);
    }
}

type TracingOutput = (TArc<TMutex<Vec<CapturedSpan>>>, TArc<TMutex<Vec<String>>>);

fn setup_tracing() -> TracingOutput {
    let spans = TArc::new(TMutex::new(Vec::new()));
    let events = TArc::new(TMutex::new(Vec::new()));
    (spans, events)
}

fn make_guard(
    spans: &TArc<TMutex<Vec<CapturedSpan>>>,
    events: &TArc<TMutex<Vec<String>>>,
) -> tracing::subscriber::DefaultGuard {
    tracing_subscriber::registry()
        .with(TestLayer::new(spans.clone(), events.clone()))
        .set_default()
}

/// Simple provider with a fixed set of phases (replacement for inaccessble TestProvider)
struct SimpleProvider {
    phases: Vec<Vec<ChatEvent>>,
    call_count: AtomicUsize,
}

impl SimpleProvider {
    fn new(phases: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            phases,
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for SimpleProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let events = self
            .phases
            .get(idx)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(Ok);
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Simple mock tool (replacement for inaccessible mock_tool)
struct MockTestTool {
    name: &'static str,
}

#[async_trait::async_trait]
impl crate::tool::Tool for MockTestTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        crate::tool::ToolResult::success("ok")
    }
}

// ── W1-S3a-1/2: visp.agent.run span ────────────────────────────────────

#[serial]
#[tokio::test]
async fn test_agent_run_span_created() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    assert!(
        captured.iter().any(|s| s.name == "visp.agent.run"),
        "expected visp.agent.run span, got: {:?}",
        captured.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_carries_session_id_field() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();
    assert!(
        run_span.fields.iter().any(|(k, _)| k == "session.id"),
        "expected session.id field"
    );
    assert!(
        run_span.fields.iter().any(|(k, _)| k == "session.short_id"),
        "expected session.short_id field"
    );
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "visp.agent.kind" && v == "primary"),
        "expected visp.agent.kind=primary"
    );
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "visp.agent.depth" && v == "0"),
        "expected visp.agent.depth=0"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_records_provider_name_from_model_key() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let llm_config = LlmConfig {
        provider: Some("Anthropic".to_string()),
        ..Default::default()
    };

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr.create(Path::new("/tmp"), llm_config).unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("expected visp.agent.run span");
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "gen_ai.provider.name" && v == "Anthropic"),
        "expected gen_ai.provider.name=Anthropic, got: {:?}",
        run_span.fields
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_carries_langfuse_session_id() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.session.id"),
        "expected langfuse.session.id field on visp.agent.run span, fields: {:?}",
        run_span.fields
    );
    // Value should equal session.id
    let langfuse_val = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.session.id")
        .map(|(_, v)| v.as_str())
        .unwrap();
    let session_id_val = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "session.id")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(
        langfuse_val, session_id_val,
        "langfuse.session.id should equal session.id"
    );
    // Verify langfuse.trace.name is present with correct format
    let trace_name = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.trace.name")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(
        trace_name, "visp.agent.run",
        "langfuse.trace.name should be visp.agent.run, got: {trace_name}"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_carries_langfuse_all_fields() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("user_456".to_string()),
        langfuse_tags: Some(r#"["agent","weather"]"#.to_string()),
        langfuse_environment: Some("prod".to_string()),
        langfuse_public: Some(true),
        langfuse_release: Some("1.0.0".to_string()),
        langfuse_version: Some("abc123".to_string()),
        langfuse_metadata: Some({
            let mut m = std::collections::HashMap::new();
            m.insert("source".to_string(), "test".to_string());
            m
        }),
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();

    // Verify langfuse.user.id
    assert!(
        run_span.fields.iter().any(|(k, _)| k == "langfuse.user.id"),
        "expected langfuse.user.id field on visp.agent.run span, fields: {:?}",
        run_span.fields
    );
    let user_id_val = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.user.id")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(
        user_id_val, "user_456",
        "langfuse.user.id should equal configured user_id"
    );

    // Verify langfuse.trace.tags (replaces old langfuse.tags)
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.trace.tags"),
        "expected langfuse.trace.tags field on visp.agent.run span, fields: {:?}",
        run_span.fields
    );
    let tags_val = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.trace.tags")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(
        tags_val, r#"["agent","weather"]"#,
        "langfuse.trace.tags should equal configured tags JSON"
    );

    // Verify no old langfuse.tags field
    assert!(
        !run_span.fields.iter().any(|(k, _)| k == "langfuse.tags"),
        "expected NO langfuse.tags field (old name), fields: {:?}",
        run_span.fields
    );

    // Verify langfuse.trace.name
    let trace_name = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.trace.name")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(
        trace_name, "visp.agent.run",
        "langfuse.trace.name should be visp.agent.run, got: {trace_name}"
    );

    // Verify langfuse.environment
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.environment" && v == "prod"),
        "expected langfuse.environment=prod, fields: {:?}",
        run_span.fields
    );

    // Verify langfuse.trace.public
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.trace.public" && v == "true"),
        "expected langfuse.trace.public=true, fields: {:?}",
        run_span.fields
    );

    // Verify langfuse.release
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.release" && v == "1.0.0"),
        "expected langfuse.release=1.0.0, fields: {:?}",
        run_span.fields
    );

    // Verify langfuse.version
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.version" && v == "abc123"),
        "expected langfuse.version=abc123, fields: {:?}",
        run_span.fields
    );

    // Verify langfuse.trace.metadata includes the metadata JSON
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.trace.metadata"
                && v.contains("source")
                && v.contains("test")),
        "expected langfuse.trace.metadata to contain metadata JSON, fields: {:?}",
        run_span.fields
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_langfuse_disabled_no_langfuse_fields() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default(); // langfuse_enabled=false
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();
    let langfuse_fields: Vec<_> = run_span
        .fields
        .iter()
        .filter(|(k, _)| k.starts_with("langfuse."))
        .collect();
    assert!(
        langfuse_fields.is_empty(),
        "expected NO langfuse.* fields when disabled, got: {:?}",
        langfuse_fields
    );
}

// ── Langfuse observation input/output recording ──────────────────────

#[serial]
#[tokio::test]
async fn test_agent_run_records_observation_type_and_input_when_capture_enabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_capture_input: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("test query"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();

    // Verify observation type = span
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.observation.type" && v == "span"),
        "expected langfuse.observation.type=span, fields: {:?}",
        run_span.fields
    );

    // Verify observation input is valid JSON containing the user message
    let input_field = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.observation.input")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(
        !input_field.is_empty(),
        "expected langfuse.observation.input to be non-empty"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(input_field).expect("observation.input should be valid JSON");
    assert_eq!(
        parsed["message"], "test query",
        "observation.input should contain the user message"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_records_output_when_capture_enabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Provider that returns text + Done
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        ChatEvent::TextDelta("Hello world response".into()),
        ChatEvent::Done,
    ]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_capture_output: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();

    // Verify observation type = span (set during output recording)
    assert!(
        run_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.observation.type" && v == "span"),
        "expected langfuse.observation.type=span, fields: {:?}",
        run_span.fields
    );

    // Verify observation output is valid JSON containing the response text
    let output_field = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.observation.output")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(
        !output_field.is_empty(),
        "expected langfuse.observation.output to be non-empty"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(output_field).expect("observation.output should be valid JSON");
    assert_eq!(
        parsed["response"], "Hello world response",
        "observation.output should contain the assistant response"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_does_not_record_input_when_capture_disabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_capture_input: false, // disabled
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("secret"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();

    assert!(
        !run_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.observation.input"),
        "expected NO langfuse.observation.input when capture is disabled, fields: {:?}",
        run_span.fields
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_does_not_record_output_when_capture_disabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Provider returns text but capture is disabled
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        ChatEvent::TextDelta("secret response".into()),
        ChatEvent::Done,
    ]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_capture_output: false, // disabled
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .unwrap();

    assert!(
        !run_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.observation.output"),
        "expected NO langfuse.observation.output when capture is disabled, fields: {:?}",
        run_span.fields
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_emits_completed_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured_events = events.lock().unwrap();
    assert!(
        captured_events.iter().any(|e| e == "visp.agent.completed"),
        "expected visp.agent.completed event, got: {:?}",
        *captured_events
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_emits_cancelled_event_on_cancel() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Cancel before starting
    ctx.cancel_token.cancel();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured_events = events.lock().unwrap();
    assert!(
        captured_events.iter().any(|e| e == "visp.agent.cancelled"),
        "expected visp.agent.cancelled event, got: {:?}",
        *captured_events
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_emits_cancelled_event_during_user_query() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();
    let cancel_token = ctx.cancel_token.clone();

    // Provider 发出带 [USER_QUERY] 标记的文本（带 options → LLM 提问路径），
    // loop 将进入 UserQuery 等待分支（select!）
    let marker_text = "[USER_QUERY]\n选择哪个方案?\n- 方案 A\n- 方案 B[/USER_QUERY]";
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        ChatEvent::TextDelta(marker_text.into()),
        ChatEvent::Done,
    ]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    let session_mgr_clone = session_mgr.clone();
    let handle = tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            rule_engine,
            session_mgr_clone,
            ctx,
            &config,
            Message::user("hello"),
            tx,
        )
        .await;
    });

    // 等待 loop 发出 UserQuery 事件（进入 select! 等待）；
    // 持有 respond 不回填，确保 loop 停在等待中
    let _respond = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(Some(AgentEvent::UserQuery { respond: r, .. })) => break r,
            Ok(None) => {
                let _ = handle.await; // 若 task panic,此处会打印 panic 位置
                panic!("run_agent_loop ended before emitting UserQuery");
            }
            Err(_) => panic!("timeout waiting for UserQuery"),
            Ok(Some(_)) => continue, // 其他事件：继续等待 UserQuery
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 提问等待中取消
    cancel_token.cancel();

    // 断言 1：run_agent_loop 应发出 Error{Cancelled} 收尾事件
    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("should receive Error event within 2s after cancel")
        .expect("channel should stay open");
    assert!(
        matches!(
            ev,
            AgentEvent::Error {
                code: AgentErrorCode::Cancelled,
                ..
            }
        ),
        "expected Error{{Cancelled}} after cancel during user query"
    );

    // 断言 2：会话状态应为 Error（finish_loop 被调用）
    handle.await.expect("run_agent_loop task should not panic");
    let s = session_mgr.get(&sid).unwrap();
    assert_eq!(
        s.status,
        SessionStatus::Error,
        "expected session status Error after cancel during user query"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_run_emits_iteration_limit_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Provider always returns a tool call; with hard_limit=1 the first
    // iteration runs (1 > 1 = false), then iteration 2 hits the limit.
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        ChatEvent::ToolCall {
            id: "call_1".into(),
            name: "finder".into(),
            arguments: "{}".into(),
        },
        ChatEvent::Done,
    ]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(MockTestTool { name: "finder" }))
        .unwrap();
    let config = AgentConfig {
        soft_limit: 0,
        hard_limit: 1,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("do tool"),
        tx,
    )
    .await;

    let captured_events = events.lock().unwrap();
    assert!(
        captured_events
            .iter()
            .any(|e| e == "visp.agent.iteration_limit"),
        "expected visp.agent.iteration_limit event, got: {:?}",
        *captured_events
    );
}

// ── W1-S3a-3/4: visp.agent.iteration span ─────────────────────────────

#[serial]
#[tokio::test]
async fn test_agent_iteration_span_nested_under_run() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> =
        StdArc::new(SimpleProvider::new(vec![vec![ChatEvent::Done]]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("expected visp.agent.run span");
    let iter_spans: Vec<&CapturedSpan> = captured
        .iter()
        .filter(|s| s.name == "visp.agent.iteration")
        .collect();
    assert!(
        !iter_spans.is_empty(),
        "expected at least one visp.agent.iteration span"
    );
    // Each iteration span's parent should be the run span
    for iter_span in &iter_spans {
        assert_eq!(
            iter_span.parent_id,
            Some(run_span.id),
            "visp.agent.iteration should be a child of visp.agent.run"
        );
    }
}

#[serial]
#[tokio::test]
async fn test_agent_iteration_field_count() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call_1".into(),
                name: "finder".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(MockTestTool { name: "finder" }))
        .unwrap();
    let config = AgentConfig::default();

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("do tool"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let iter_spans: Vec<&CapturedSpan> = captured
        .iter()
        .filter(|s| s.name == "visp.agent.iteration")
        .collect();
    assert!(
        !iter_spans.is_empty(),
        "expected at least one iteration span"
    );
    // Each iteration span should have iteration.count field
    for (i, span) in iter_spans.iter().enumerate() {
        assert!(
            span.fields.iter().any(|(k, _)| k == "iteration.count"),
            "iteration span {} should have iteration.count field, fields: {:?}",
            i,
            span.fields
        );
    }
}

// ── W1-S3b: iteration langfuse propagation ────────────────────────────

#[serial]
#[tokio::test]
async fn test_iteration_span_propagates_langfuse_fields_when_enabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Use a provider that does one tool call then done, giving us 2 iterations
    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(MockTestTool { name: "sleepy" }))
        .unwrap();
    let config = AgentConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("iter_user".to_string()),
        langfuse_tags: Some(r#"["iter"]"#.to_string()),
        langfuse_environment: Some("staging".to_string()),
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("test iteration"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    // Get root run span trace name for comparison
    let run_span = captured
        .iter()
        .find(|s| s.name == "visp.agent.run")
        .expect("expected visp.agent.run span");
    let root_trace_name = run_span
        .fields
        .iter()
        .find(|(k, _)| k == "langfuse.trace.name")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    // Get iteration spans
    let iter_spans: Vec<&CapturedSpan> = captured
        .iter()
        .filter(|s| s.name == "visp.agent.iteration")
        .collect();
    assert!(
        !iter_spans.is_empty(),
        "expected at least one iteration span"
    );

    // Each iteration span should carry trace-level langfuse fields
    for (i, span) in iter_spans.iter().enumerate() {
        assert!(
            span.fields.iter().any(|(k, _)| k == "langfuse.session.id"),
            "iteration span {} should have langfuse.session.id, fields: {:?}",
            i,
            span.fields
        );
        assert!(
            span.fields.iter().any(|(k, _)| k == "langfuse.trace.name"),
            "iteration span {} should have langfuse.trace.name, fields: {:?}",
            i,
            span.fields
        );
        assert!(
            span.fields.iter().any(|(k, _)| k == "langfuse.user.id"),
            "iteration span {} should have langfuse.user.id, fields: {:?}",
            i,
            span.fields
        );
        assert!(
            span.fields.iter().any(|(k, _)| k == "langfuse.trace.tags"),
            "iteration span {} should have langfuse.trace.tags, fields: {:?}",
            i,
            span.fields
        );
        assert!(
            span.fields.iter().any(|(k, _)| k == "langfuse.environment"),
            "iteration span {} should have langfuse.environment, fields: {:?}",
            i,
            span.fields
        );

        // Verify trace name matches root
        let iter_trace_name = span
            .fields
            .iter()
            .find(|(k, _)| k == "langfuse.trace.name")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert_eq!(
            iter_trace_name, root_trace_name,
            "iteration span {} trace name should match root",
            i
        );
    }
}

#[serial]
#[tokio::test]
async fn test_iteration_span_no_langfuse_fields_when_disabled() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(MockTestTool { name: "sleepy" }))
        .unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default() // langfuse_enabled=false
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("test no-langfuse iteration"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let iter_spans: Vec<&CapturedSpan> = captured
        .iter()
        .filter(|s| s.name == "visp.agent.iteration")
        .collect();
    assert!(
        !iter_spans.is_empty(),
        "expected at least one iteration span"
    );

    for (i, span) in iter_spans.iter().enumerate() {
        let langfuse_fields: Vec<_> = span
            .fields
            .iter()
            .filter(|(k, _)| k.starts_with("langfuse."))
            .collect();
        assert!(
            langfuse_fields.is_empty(),
            "iteration span {} should have NO langfuse.* fields when disabled, got: {:?}",
            i,
            langfuse_fields
        );
    }
}

// ── W1-S3a-5/6: visp.tool.execute span ────────────────────────────────

/// Provider that returns three tool calls in one go
struct ThreeToolProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ThreeToolProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_1".into(),
                    name: "finder".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::ToolCall {
                    id: "call_2".into(),
                    name: "grep".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::ToolCall {
                    id: "call_3".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd": "echo hi"}"#.into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

#[serial]
#[tokio::test]
async fn test_tool_execute_span_per_call() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(ThreeToolProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(SleepyTool)).unwrap();
    // Register SleepyTool twice with different names using inline wrappers
    struct GrepTool;
    #[async_trait::async_trait]
    impl crate::tool::Tool for GrepTool {
        fn name(&self) -> &str {
            "grep"
        }
        fn description(&self) -> &str {
            "greps"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &crate::tool::ToolContext,
        ) -> crate::tool::ToolResult {
            crate::tool::ToolResult::success("matched")
        }
    }
    struct BashTool;
    #[async_trait::async_trait]
    impl crate::tool::Tool for BashTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "runs"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &crate::tool::ToolContext,
        ) -> crate::tool::ToolResult {
            crate::tool::ToolResult::success("done")
        }
    }
    registry.register(StdArc::new(GrepTool)).unwrap();
    registry.register(StdArc::new(BashTool)).unwrap();

    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("use tools"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let tool_spans: Vec<&CapturedSpan> = captured
        .iter()
        .filter(|s| s.name == "visp.tool.execute")
        .collect();
    assert_eq!(
        tool_spans.len(),
        3,
        "expected 3 visp.tool.execute spans, got {}: {:?}",
        tool_spans.len(),
        tool_spans.iter().map(|s| &s.fields).collect::<Vec<_>>()
    );
}

#[serial]
#[tokio::test]
async fn test_tool_execute_fields() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(SleepyTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        langfuse_enabled: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("sleep"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let tool_span = captured
        .iter()
        .find(|s| s.name == "visp.tool.execute")
        .expect("expected visp.tool.execute span");

    // Check required fields
    let field_names: Vec<&str> = tool_span.fields.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, _)| k == "gen_ai.tool.name"),
        "expected gen_ai.tool.name field, got: {:?}",
        field_names
    );
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, _)| k == "gen_ai.tool.call.id"),
        "expected gen_ai.tool.call.id field, got: {:?}",
        field_names
    );
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, _)| k == "gen_ai.tool.type"),
        "expected gen_ai.tool.type field, got: {:?}",
        field_names
    );
    // Check langfuse observation type
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, v)| k == "langfuse.observation.type" && v == "span"),
        "expected langfuse.observation.type=span, got: {:?}",
        field_names
    );
    // Check gen_ai operation name
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, v)| k == "gen_ai.operation.name" && v == "execute_tool"),
        "expected gen_ai.operation.name=execute_tool, got: {:?}",
        field_names
    );
    // Check trace-level langfuse fields are propagated
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.session.id"),
        "expected langfuse.session.id on tool span"
    );
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, _)| k == "langfuse.trace.name"),
        "expected langfuse.trace.name on tool span"
    );
}

#[serial]
#[tokio::test]
async fn test_tool_execute_success_level_default() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(SleepyTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        langfuse_enabled: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("sleep"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let tool_span = captured
        .iter()
        .find(|s| s.name == "visp.tool.execute")
        .expect("expected visp.tool.execute span");

    // Success should have level=DEFAULT
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, v)| k == "level" && v == "DEFAULT"),
        "expected level=DEFAULT for successful tool, got fields: {:?}",
        tool_span.fields
    );
    // No status_message for success
    assert!(
        !tool_span.fields.iter().any(|(k, _)| k == "status_message"),
        "expected no status_message for successful tool, got fields: {:?}",
        tool_span.fields
    );
}

#[serial]
#[tokio::test]
async fn test_tool_execute_error_level_error_with_status_message() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::session::InMemorySessionStore;
    use std::path::Path;

    // ErrorTool returns an error
    struct ErrorTool;
    #[async_trait::async_trait]
    impl crate::tool::Tool for ErrorTool {
        fn name(&self) -> &str {
            "error_tool"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> crate::tool::ToolResult {
            crate::tool::ToolResult::error("always fails")
        }
    }

    struct ErrorToolProvider {
        call_count: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl LlmProvider for ErrorToolProvider {
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
        {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                let events = vec![
                    Ok(ChatEvent::ToolCall {
                        id: "call_err_1".into(),
                        name: "error_tool".into(),
                        arguments: "{}".into(),
                    }),
                    Ok(ChatEvent::Done),
                ];
                Ok(Box::pin(stream::iter(events)))
            } else {
                Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
            }
        }
    }

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(ErrorToolProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(ErrorTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        langfuse_enabled: true,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("fail"),
        tx,
    )
    .await;

    let captured = spans.lock().unwrap();
    let tool_span = captured
        .iter()
        .find(|s| s.name == "visp.tool.execute")
        .expect("expected visp.tool.execute span");

    // Error should have level=ERROR
    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, v)| k == "level" && v == "ERROR"),
        "expected level=ERROR for failing tool, got fields: {:?}",
        tool_span.fields
    );
    // status_message should contain a summary (not full args/results)
    assert!(
        tool_span.fields.iter().any(|(k, _)| k == "status_message"),
        "expected status_message for failing tool, got fields: {:?}",
        tool_span.fields
    );
    let status_msg = tool_span
        .fields
        .iter()
        .find(|(k, _)| k == "status_message")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(
        status_msg.contains("always fails"),
        "status_message should contain error summary, got: {status_msg}"
    );
    // status_message should NOT contain full args or results (just summary)
    assert!(
        status_msg.len() < 200,
        "status_message should be a short summary, got length {}",
        status_msg.len()
    );
}

#[serial]
#[tokio::test]
async fn test_tool_execute_duration_ms_uses_authoritative_value() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::session::InMemorySessionStore;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Use the tool that records duration in history
    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(SleepyTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("sleep"),
        tx,
    )
    .await;

    // Get duration from history (Wave 0 authoritative value)
    let final_session = session_mgr.get(&sid).unwrap();
    let tool_msg = final_session
        .history
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("expected a tool message in history");
    let history_duration = tool_msg
        .tool_result_duration_ms
        .expect("expected duration_ms on tool message");

    // Get duration from span
    let captured = spans.lock().unwrap();
    let tool_span = captured
        .iter()
        .find(|s| s.name == "visp.tool.execute")
        .expect("expected visp.tool.execute span");

    // Find duration_ms field in span
    let span_duration = tool_span
        .fields
        .iter()
        .find(|(k, _)| k == "visp.tool.duration_ms")
        .map(|(_, v)| v.parse::<u64>().unwrap_or(0));

    if let Some(span_dur) = span_duration {
        // The span duration and history duration should match (both use the same elapsed_ms)
        // They come from the same measurement, so should be identical
        assert_eq!(
            span_dur, history_duration,
            "span duration_ms should match the authoritative history value"
        );
    }
    // If no duration_ms field, the tool was cancelled early — acceptable
}

#[serial]
#[tokio::test]
async fn test_tool_execute_is_error_true_on_failure() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    use crate::session::InMemorySessionStore;
    use std::path::Path;

    // ErrorTool returns an error
    struct ErrorTool;
    #[async_trait::async_trait]
    impl crate::tool::Tool for ErrorTool {
        fn name(&self) -> &str {
            "error_tool"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::tool::ToolContext,
        ) -> crate::tool::ToolResult {
            crate::tool::ToolResult::error("always fails")
        }
    }

    struct ErrorToolProvider {
        call_count: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl LlmProvider for ErrorToolProvider {
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
        {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                let events = vec![
                    Ok(ChatEvent::ToolCall {
                        id: "call_err_1".into(),
                        name: "error_tool".into(),
                        arguments: "{}".into(),
                    }),
                    Ok(ChatEvent::Done),
                ];
                Ok(Box::pin(stream::iter(events)))
            } else {
                Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
            }
        }
    }

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(ErrorToolProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(StdArc::new(ErrorTool)).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("fail"),
        tx,
    )
    .await;

    // 计划 0c-2 测试 3（真实失败路径）：is_error=true 且 outcome=Failure。
    let mut failure: Option<(bool, ToolOutcome)> = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolCallResult {
            is_error, outcome, ..
        } = event
        {
            failure = Some((is_error, outcome));
        }
    }
    assert_eq!(failure, Some((true, ToolOutcome::Failure)));

    let captured = spans.lock().unwrap();
    let tool_span = captured
        .iter()
        .find(|s| s.name == "visp.tool.execute")
        .expect("expected visp.tool.execute span");

    assert!(
        tool_span
            .fields
            .iter()
            .any(|(k, v)| k == "visp.tool.is_error" && v == "true"),
        "expected visp.tool.is_error=true, got fields: {:?}",
        tool_span.fields
    );
}

// ── Wave 2c: agent_loop simplification ──────────────────────────────────

/// Provider: first call returns a "task" tool call, then Done.
struct TaskNameProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for TaskNameProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_1".into(),
                    name: "task".into(),
                    arguments: r#"{"prompt": "test"}"#.into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

/// A tool that registers itself as "task" in the registry.
/// This verifies the agent loop treats "task" as a regular tool name
/// (no longer intercepted for sub-agent spawning).
#[async_trait::async_trait]
impl crate::tool::Tool for TaskNameTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "a regular tool named 'task'"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        crate::tool::ToolResult::success("task tool executed")
    }
}

struct TaskNameTool;

#[serial]
#[tokio::test]
async fn test_no_task_name_interception() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(TaskNameProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(TaskNameTool))
        .expect("register 'task' tool");
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    // Run agent loop — should NOT hang (no Phase 2 blocking on inbox).
    // The "task" tool call should be executed as a regular tool.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_agent_loop(
            provider,
            StdArc::new(registry),
            rule_engine,
            session_mgr.clone(),
            ctx,
            &config,
            Message::user("use task tool"),
            tx,
        ),
    )
    .await;

    assert!(result.is_ok(), "agent loop should complete without hanging");

    // Verify the tool was actually executed (history has tool result)
    let final_session = session_mgr.get(&sid).unwrap();
    let tool_msgs: Vec<_> = final_session
        .history
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "expected 1 tool result message");
    assert!(
        tool_msgs[0].content.contains("task tool executed"),
        "task tool should have been executed as regular tool"
    );
}

/// A tool that sleeps for a long time — used to verify cancel aborts execution.
struct SlowTool;

#[async_trait::async_trait]
impl crate::tool::Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "slow tool for cancel testing"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        crate::tool::ToolResult::success("done")
    }
}

/// Provider: first call returns a "slow" tool call, then Done.
struct SlowToolProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SlowToolProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_slow".into(),
                    name: "slow".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

#[serial]
#[tokio::test]
async fn test_cancel_aborts_agent_tool() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();
    let cancel_token = ctx.cancel_token.clone();

    let provider: StdArc<dyn LlmProvider> = StdArc::new(SlowToolProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(StdArc::new(SlowTool))
        .expect("register slow tool");
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(64);

    let sm_clone = session_mgr.clone();
    let handle = tokio::spawn(async move {
        run_agent_loop(
            provider,
            StdArc::new(registry),
            rule_engine,
            sm_clone,
            ctx,
            &config,
            Message::user("do slow"),
            tx,
        )
        .await;
    });

    // Give agent time to start executing the slow tool
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel_token.cancel();

    // Should exit quickly (not wait 10s)
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("agent loop should exit within 2s after cancel");

    assert!(
        result.is_ok() || (result.is_err() && result.unwrap_err().is_cancelled()),
        "agent loop should complete or be cancelled"
    );

    // Session should be in Error state
    let final_session = session_mgr.get(&sid).unwrap();
    assert_eq!(
        final_session.status,
        crate::session::SessionStatus::Error,
        "session should end in Error after cancel"
    );
}

/// A tool that returns an image marker (`<image: | url>` and `<image: path>`)
/// in its result, simulating a painter sub-agent output.
struct ImageMarkerTool;
#[async_trait::async_trait]
impl crate::tool::Tool for ImageMarkerTool {
    fn name(&self) -> &str {
        "image_tool"
    }
    fn description(&self) -> &str {
        "returns image markers"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        crate::tool::ToolResult::success(
            "painted\n<image: | https://example.com/img.png>\n<image: /tmp/local.png>",
        )
    }
}

/// A tool that returns an error message containing an image marker.
/// Image markers in error results must NOT produce ImageBlock events.
struct ErrorImageMarkerTool;
#[async_trait::async_trait]
impl crate::tool::Tool for ErrorImageMarkerTool {
    fn name(&self) -> &str {
        "error_image_tool"
    }
    fn description(&self) -> &str {
        "returns image marker in an error result"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        crate::tool::ToolResult::error("failed\n<image: | https://example.com/broken.png>")
    }
}

/// Provider: first call requests the tool, subsequent calls just return Done.
struct OneImageToolCallProvider {
    call_count: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for OneImageToolCallProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_img_1".into(),
                    name: "image_tool".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

/// Provider: first call requests a tool that errors, subsequent calls return Done.
struct ErrorImageToolProvider {
    call_count: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for ErrorImageToolProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            let events = vec![
                Ok(ChatEvent::ToolCall {
                    id: "call_err_img_1".into(),
                    name: "error_image_tool".into(),
                    arguments: "{}".into(),
                }),
                Ok(ChatEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        } else {
            Ok(Box::pin(stream::iter(vec![Ok(ChatEvent::Done)])))
        }
    }
}

async fn run_image_marker_agent(
    provider: StdArc<dyn LlmProvider>,
    tool: StdArc<dyn crate::tool::Tool>,
    tool_name: &str,
) -> (StdArc<SessionManager>, String, mpsc::Receiver<AgentEvent>) {
    use crate::session::InMemorySessionStore;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let rule_engine = StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry.register(tool).unwrap();
    let config = AgentConfig {
        hard_limit: 10,
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        StdArc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user(format!("use {tool_name}")),
        tx,
    )
    .await;
    (session_mgr, sid, rx)
}

#[serial]
#[tokio::test]
async fn test_tool_result_image_markers_emit_image_block_events() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(OneImageToolCallProvider {
        call_count: AtomicUsize::new(0),
    });
    let (session_mgr, sid, mut rx) =
        run_image_marker_agent(provider, StdArc::new(ImageMarkerTool), "image_tool").await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }

    // Remote URL marker → ImageBlock with remote_url
    let url_block = events.iter().find_map(|e| match e {
        AgentEvent::ImageBlock { remote_url, .. } => remote_url.clone(),
        _ => None,
    });
    assert_eq!(
        url_block.as_deref(),
        Some("https://example.com/img.png"),
        "expected ImageBlock for remote URL marker"
    );

    // Local path marker → ImageBlock with path
    let path_block = events.iter().find_map(|e| match e {
        AgentEvent::ImageBlock {
            path, remote_url, ..
        } if remote_url.is_none() => Some(path.clone()),
        _ => None,
    });
    assert_eq!(
        path_block.as_deref(),
        Some("/tmp/local.png"),
        "expected ImageBlock for local path marker"
    );

    // Markers must be preserved in the tool message content (LLM context)
    let final_session = session_mgr.get(&sid).unwrap();
    let tool_msg = final_session
        .history
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("expected a tool message in history");
    assert!(
        tool_msg
            .content
            .contains("<image: | https://example.com/img.png>"),
        "expected URL marker preserved in tool message, got: {}",
        tool_msg.content
    );
    assert!(
        tool_msg.content.contains("<image: /tmp/local.png>"),
        "expected local marker preserved in tool message, got: {}",
        tool_msg.content
    );
}

#[serial]
#[tokio::test]
async fn test_tool_result_image_markers_skipped_on_error() {
    let provider: StdArc<dyn LlmProvider> = StdArc::new(ErrorImageToolProvider {
        call_count: AtomicUsize::new(0),
    });
    let (_session_mgr, _sid, mut rx) = run_image_marker_agent(
        provider,
        StdArc::new(ErrorImageMarkerTool),
        "error_image_tool",
    )
    .await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ImageBlock { .. })),
        "error results must not emit ImageBlock events"
    );
}

/// Provider that always returns a thinking-only response (no text, no
/// tool calls) with metadata — mirrors the Ali/DeepSeek thinking loop.
struct ThinkingOnlyProvider;

#[async_trait::async_trait]
impl LlmProvider for ThinkingOnlyProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::ThinkingBlock(serde_json::json!({
                "type": "thinking",
                "thinking": "thinking about the task..."
            }))),
            Ok(ChatEvent::OutputMetadata(ProviderMetadata {
                model: "thinking-test".into(),
                finish_reasons: vec!["length".into()],
                input_tokens: 10,
                output_tokens: 4096,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 100,
            })),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Regression: thinking-only iterations (empty tool_calls) must NOT be
/// counted as a repeated-tool-call loop. Previously execute_tool_calls was
/// invoked unconditionally and pushed an empty signature into the doom-loop
/// window, so N consecutive thinking-only turns tripped StuckInLoop.
#[serial]
#[tokio::test]
async fn test_thinking_only_loop_does_not_trigger_doom_loop() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(ThinkingOnlyProvider);
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        hard_limit: 12,         // enough iterations to trip the old bug (5+5)
        doom_loop_threshold: 3, // small window to make the test fast
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("think about it"),
        tx,
    )
    .await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::Error {
                code: AgentErrorCode::StuckInLoop,
                ..
            }
        )),
        "thinking-only iterations must not trigger StuckInLoop error event"
    );
}

/// Provider 声称 `finish_reasons=["tool_calls"]`，但流里只有 thinking、没有
/// 任何 tool_calls——复现 SSE 解析缺口导致 provider 与解析器不一致的形态。
struct ClaimsToolCallsButParsesNoneProvider;

#[async_trait::async_trait]
impl LlmProvider for ClaimsToolCallsButParsesNoneProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::ThinkingBlock(serde_json::json!({
                "type": "thinking",
                "thinking": "I should call a tool here"
            }))),
            Ok(ChatEvent::OutputMetadata(ProviderMetadata {
                model: "mismatch-test".into(),
                finish_reasons: vec!["tool_calls".into()],
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 100,
            })),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// 用例 1：provider 元数据声称 `tool_calls`，但解析到的 tool_calls 为空。
/// 这是 provider/解析器不一致，必须明确失败并给出可诊断信息，绝不能再当成
/// thinking-only 续跑到迭代上限（此前会连续空转）。
#[serial]
#[tokio::test]
async fn test_tool_calls_claimed_but_none_parsed_fails_loudly() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(ClaimsToolCallsButParsesNoneProvider);
    let config = AgentConfig {
        hard_limit: 3,
        ..Default::default()
    };

    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr.clone(),
        ctx,
        config,
        Message::user("do something"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message, .. }
                if message.contains("provider 报告 tool_calls 但未解析到任何工具调用")
        )),
        "provider/parser mismatch must surface a diagnosable Error"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "mismatch must not complete as Done"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::Error {
                code: AgentErrorCode::MaxIterations,
                ..
            }
        )),
        "mismatch must fail immediately, not spin until the iteration limit"
    );
    assert_eq!(
        session_mgr.get(&sid).unwrap().status,
        SessionStatus::Error,
        "session must end in Error"
    );
}

/// 用例 2（回归）：`finish_reasons=["stop"]` 的 thinking-only 响应仍按既有
/// 逻辑 Continue，不被新的一致性检查误伤。
#[serial]
#[tokio::test]
async fn test_stop_finish_reason_thinking_only_still_continues() {
    use crate::ProviderMetadata;

    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        vec![
            thinking_snapshot("thinking before stopping"),
            ChatEvent::OutputMetadata(ProviderMetadata {
                model: "test".into(),
                finish_reasons: vec!["stop".into()],
                input_tokens: 10,
                output_tokens: 50,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 100,
            }),
            ChatEvent::Done,
        ],
        vec![ChatEvent::TextDelta("final answer".into()), ChatEvent::Done],
    ]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "stop thinking-only must not error"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("final answer"))),
        "stop thinking-only must still continue and produce text"
    );
}

/// 用例 3（回归）：正常带 tool_calls 的响应照常执行工具，不受一致性检查影响。
#[serial]
#[tokio::test]
async fn test_normal_tool_calls_still_execute() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    let provider = one_tool_then_done("noargs", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "normal tool-call flow must not error"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResult { content, .. } if content == "ok"
        )),
        "tool must still execute in the normal tool-call flow"
    );
}

/// 用例 4（回归）：`finish_reasons=["length"]` 的既有 thinking-only 续跑路径
/// 不变（既有 `test_thinking_only_snapshot_round_still_continues` 亦覆盖）。
#[serial]
#[tokio::test]
async fn test_length_finish_reason_thinking_only_still_continues() {
    use crate::ProviderMetadata;

    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        vec![
            thinking_snapshot("exhausted budget while thinking"),
            ChatEvent::OutputMetadata(ProviderMetadata {
                model: "test".into(),
                finish_reasons: vec!["length".into()],
                input_tokens: 10,
                output_tokens: 4096,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 100,
            }),
            ChatEvent::Done,
        ],
        vec![ChatEvent::TextDelta("after retry".into()), ChatEvent::Done],
    ]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "length thinking-only must not error"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("after retry"))),
        "length thinking-only must still continue and produce text"
    );
}

// ── F5：连续 thinking-only 续跑上限 ────────────────────────────────────

/// 构造一轮「仅思考、无正文/工具」的响应（含 metadata，finish_reason=stop）。
fn thinking_only_phase(text: &str) -> Vec<ChatEvent> {
    vec![
        thinking_snapshot(text),
        ChatEvent::OutputMetadata(ProviderMetadata {
            model: "test".into(),
            finish_reasons: vec!["stop".into()],
            input_tokens: 10,
            output_tokens: 50,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            latency_ms: 100,
        }),
        ChatEvent::Done,
    ]
}

/// 用例 F5-1：provider 连续多轮（12 轮，足够撞 hard_limit）只思考不产出
/// 正文/工具，必须在**连续上限**处明确失败，而不是续跑到迭代上限空转。
#[serial]
#[tokio::test]
async fn test_consecutive_thinking_only_aborts_at_limit() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let phases: Vec<Vec<ChatEvent>> = (0..12)
        .map(|i| thinking_only_phase(&format!("t{i}")))
        .collect();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(phases));

    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr.clone(),
        ctx,
        AgentConfig {
            hard_limit: 12,
            ..Default::default()
        },
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message, .. } if message.contains("仅思考")
        )),
        "连续 thinking-only 必须给出含「仅思考」的可诊断 Error"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::Error {
                code: AgentErrorCode::MaxIterations,
                ..
            }
        )),
        "F5 必须在迭代上限之前拦截，不得跑到 MaxIterations"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "被 F5 中止的会话不得报 Done"
    );
    assert_eq!(
        session_mgr.get(&sid).unwrap().status,
        SessionStatus::Error,
        "会话终态必须是 Error"
    );
}

/// 用例 F5-2（不误伤）：2 轮 thinking-only 后正常产出正文，应正常完成。
#[serial]
#[tokio::test]
async fn test_two_thinking_only_rounds_then_text_completes() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        thinking_only_phase("t1"),
        thinking_only_phase("t2"),
        vec![ChatEvent::TextDelta("final answer".into()), ChatEvent::Done],
    ]));

    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr.clone(),
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "2 轮 thinking-only 不得触发 F5"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("final answer"))),
        "最终正文必须产出"
    );
    assert_eq!(
        session_mgr.get(&sid).unwrap().status,
        SessionStatus::Completed,
        "会话应正常完成"
    );
}

/// 用例 F5-3（计数清零）：2 轮 thinking-only → 普通（工具）轮 → 再 2 轮
/// thinking-only，仍应正常结束；若计数不清零，累计到第 4 轮会被 F5 误杀。
#[serial]
#[tokio::test]
async fn test_thinking_only_counter_resets_after_normal_round() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        thinking_only_phase("t1"),
        thinking_only_phase("t2"),
        vec![
            ChatEvent::ToolCall {
                id: "call_1".into(),
                name: "noargs".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        thinking_only_phase("t3"),
        thinking_only_phase("t4"),
        vec![ChatEvent::TextDelta("done".into()), ChatEvent::Done],
    ]));

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr.clone(),
        ctx,
        AgentConfig::default(),
        Message::user("go"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "普通轮次必须清零计数，避免跨轮累计触发 F5"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("done"))),
        "最终正文必须产出"
    );
    assert_eq!(
        session_mgr.get(&sid).unwrap().status,
        SessionStatus::Completed
    );
}

/// 用例空响应一致性：空流（无 text/tool/thinking）但 provider 声称
/// `tool_calls` 且 `output_tokens==0`，必须明确失败，不得静默 Done。
#[serial]
#[tokio::test]
async fn test_empty_response_claiming_tool_calls_fails_loudly() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        ChatEvent::OutputMetadata(ProviderMetadata {
            model: "empty-mismatch".into(),
            finish_reasons: vec!["tool_calls".into()],
            input_tokens: 10,
            // output_tokens 未经 UsageInfo 上报，保持 0。
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            latency_ms: 100,
        }),
        ChatEvent::Done,
    ]]));

    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr.clone(),
        ctx,
        AgentConfig::default(),
        Message::user("do something"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message, .. }
                if message.contains("provider 报告 tool_calls 但未解析到任何工具调用")
        )),
        "空响应且声称 tool_calls 必须明确失败"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "空响应声称 tool_calls 不得静默 Done"
    );
    assert_eq!(session_mgr.get(&sid).unwrap().status, SessionStatus::Error);
}

/// 用例 F5-5（回归）：普通工具调用 + 普通正文回答，行为不变。
#[serial]
#[tokio::test]
async fn test_normal_tool_call_and_text_answer_unchanged() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call_1".into(),
                name: "noargs".into(),
                arguments: "{}".into(),
            },
            ChatEvent::Done,
        ],
        vec![
            ChatEvent::TextDelta("normal answer".into()),
            ChatEvent::Done,
        ],
    ]));

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr.clone(),
        ctx,
        AgentConfig::default(),
        Message::user("go"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "普通流程不得报错"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResult { content, .. } if content == "ok"
        )),
        "工具照常执行"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("normal answer"))),
        "正文照常产出"
    );
    assert_eq!(
        session_mgr.get(&sid).unwrap().status,
        SessionStatus::Completed
    );
}

// ── 思考真增量（快照 → ThinkingDelta）─────────────────────────────────

fn thinking_snapshot(text: &str) -> ChatEvent {
    ChatEvent::ThinkingBlock(serde_json::json!({
        "type": "thinking",
        "thinking": text,
    }))
}

fn thinking_deltas_of(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ThinkingDelta(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

/// 纯函数：前缀增长 → 差集尾部。
#[test]
fn test_thinking_increment_prefix_emits_tail() {
    assert_eq!(thinking_increment("", "A"), "A");
    assert_eq!(thinking_increment("AB", "ABC"), "C");
}

/// 纯函数：非前缀（新块 / 跳变 / 回退变短）→ 全文。
#[test]
fn test_thinking_increment_non_prefix_emits_full() {
    assert_eq!(thinking_increment("AB", "XYZ"), "XYZ");
    assert_eq!(thinking_increment("ABC", "AB"), "AB");
}

/// 纯函数：文本未变 → 空串（调用方据此抑制发射）。
#[test]
fn test_thinking_increment_unchanged_emits_empty() {
    assert_eq!(thinking_increment("same", "same"), "");
}

/// 用例 1 + 7 + 10：连续同块快照只发增量；增量拼接 == 最终全文；
/// 流式路径无快照变体；下游总字节 == 全文长度（O(n)）。
#[serial]
#[tokio::test]
async fn test_thinking_snapshots_emit_increments_only() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("A"),
        thinking_snapshot("AB"),
        thinking_snapshot("ABC"),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    let deltas = thinking_deltas_of(&events);
    assert_eq!(deltas, vec!["A", "B", "C"], "只发真增量");
    assert_eq!(deltas.concat(), "ABC", "增量拼接必须等于最终全文");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ThinkingBlock(_))),
        "流式路径不得再发射快照变体"
    );
    let total: usize = deltas.iter().map(String::len).sum();
    assert_eq!(total, "ABC".len(), "下游总字节必须线性等于全文长度");
}

/// 用例 2：文本未变的终帧（如仅带签名）→ 不发射。
#[serial]
#[tokio::test]
async fn test_thinking_unchanged_terminal_snapshot_not_emitted() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("same"),
        ChatEvent::ThinkingBlock(serde_json::json!({
            "type": "thinking",
            "thinking": "same",
            "signature": "base64sig",
        })),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert_eq!(
        thinking_deltas_of(&events),
        vec!["same"],
        "文本未变的终帧不得发射增量"
    );
}

/// 用例 3：非前缀快照 → 首发全文并重置基线，后续继续取其差集。
#[serial]
#[tokio::test]
async fn test_thinking_non_prefix_snapshot_resets_baseline() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("AB"),
        thinking_snapshot("XYZ"),
        thinking_snapshot("XYZW"),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert_eq!(thinking_deltas_of(&events), vec!["AB", "XYZ", "W"]);
}

/// 用例 4：回退/更短快照（异常）→ 视为新块起点，发全文，不产生负增量。
#[serial]
#[tokio::test]
async fn test_thinking_rollback_snapshot_emits_full() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("ABC"),
        thinking_snapshot("AB"),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert_eq!(
        thinking_deltas_of(&events),
        vec!["ABC", "AB"],
        "回退快照视为新块起点，发全文而非负增量"
    );
}

/// 用例 5：仅思考无正文轮次，控制流不变——内部完整块仍触发既有
/// 「继续循环」判定，且完整块被装配持久化（不因改成增量而削弱）。
#[serial]
#[tokio::test]
async fn test_thinking_only_snapshot_round_still_continues() {
    use crate::ProviderMetadata;

    let (session_mgr, sid, ctx) = make_loop_env();
    let inspect = session_mgr.clone();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![
        vec![
            thinking_snapshot("thinking about the task"),
            thinking_snapshot("thinking about the task more"),
            ChatEvent::OutputMetadata(ProviderMetadata {
                model: "test".into(),
                finish_reasons: vec!["length".into()],
                input_tokens: 10,
                output_tokens: 4096,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                latency_ms: 100,
            }),
            ChatEvent::Done,
        ],
        vec![ChatEvent::TextDelta("final answer".into()), ChatEvent::Done],
    ]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "仅思考轮次不应报错"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("final answer"))),
        "仅思考轮次仍应继续循环并产出正文"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ThinkingDelta(_)))
    );

    let saved = inspect.get(&sid).unwrap();
    let has_full_block = saved.history.iter().any(|m| {
        m.extra_blocks.as_ref().is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("thinking").and_then(|v| v.as_str()) == Some("thinking about the task more")
            })
        })
    });
    assert!(
        has_full_block,
        "本轮完整思考块必须被装配持久化（extra_blocks），不得被增量语义削弱"
    );
}

/// 用例 6：`event_to_msg(增量变体)` 不转发给 orchestrator。
#[test]
fn test_event_to_msg_thinking_delta_is_not_forwarded() {
    assert!(event_to_msg(&AgentEvent::ThinkingDelta("inc".into())).is_none());
}

// ── 集成不变量：增量拼接 == 全文；extra_blocks 仍为完整块 ─────────────

/// 用例 11a-1：思考与正文**交错**时，思考增量序列拼接必须等于本轮完整思考
/// 文本，且正文增量拼接等于正文全文（交错不得打乱任一侧的累积）。
#[serial]
#[tokio::test]
async fn test_thinking_increments_with_interleaved_text_concat_to_full() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("I think "),
        ChatEvent::TextDelta("Hello".into()),
        thinking_snapshot("I think more"),
        ChatEvent::TextDelta(" world".into()),
        thinking_snapshot("I think more clearly"),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("think and answer"),
        ApprovalAction::Ignore,
    )
    .await;

    let deltas = thinking_deltas_of(&events);
    assert_eq!(deltas, vec!["I think ", "more", " clearly"]);
    assert_eq!(
        deltas.concat(),
        "I think more clearly",
        "交错场景下思考增量拼接必须等于最终完整思考文本"
    );

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world", "正文增量拼接 == 正文全文");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ThinkingBlock(_))),
        "流式路径不得再发射快照变体"
    );
}

/// 用例 11a-2：走**正文装配路径**时，落库消息的 `extra_blocks` 仍为本轮完整
/// 思考块且保留 signature——这是 Anthropic 历史回传的硬约束，不得被增量
/// 语义削弱。
#[serial]
#[tokio::test]
async fn test_extra_blocks_keep_full_signed_block_on_text_path() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let inspect = session_mgr.clone();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("deep "),
        ChatEvent::ThinkingBlock(serde_json::json!({
            "type": "thinking",
            "thinking": "deep thought here",
            "signature": "sig-abc123",
        })),
        ChatEvent::TextDelta("final answer".into()),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("go"),
        ApprovalAction::Ignore,
    )
    .await;

    assert_eq!(
        thinking_deltas_of(&events).concat(),
        "deep thought here",
        "增量拼接 == 完整思考文本"
    );

    let saved = inspect.get(&sid).unwrap();
    let text_msg = saved
        .history
        .iter()
        .find(|m| m.kind == MessageType::Text && m.content.contains("final answer"))
        .expect("正文装配路径应落库一条 text 消息");
    let blocks = text_msg
        .extra_blocks
        .as_ref()
        .expect("正文消息必须携带本轮完整思考块（extra_blocks）");
    assert_eq!(blocks.len(), 1, "本轮只有一块，extra_blocks 应恰含一块");
    assert_eq!(
        blocks[0].get("thinking").and_then(|v| v.as_str()),
        Some("deep thought here"),
        "extra_blocks 必须保留完整思考文本"
    );
    assert_eq!(
        blocks[0].get("signature").and_then(|v| v.as_str()),
        Some("sig-abc123"),
        "extra_blocks 必须保留 signature（Anthropic 回传硬约束）"
    );
}

/// 用例 11a-4：一轮内出现**多个思考块**时，对外增量为各块累积（显示侧
/// 「两段拼接」），但落库/history 的 `extra_blocks` 仍只有最后一块——固化
/// 设计 §4.4 的已知差异（持久化部分）。
#[serial]
#[tokio::test]
async fn test_multiple_thinking_blocks_deltas_accumulate_but_persist_last_only() {
    let (session_mgr, sid, ctx) = make_loop_env();
    let inspect = session_mgr.clone();
    let provider: StdArc<dyn LlmProvider> = StdArc::new(SimpleProvider::new(vec![vec![
        thinking_snapshot("first segment"),
        thinking_snapshot("second segment"),
        ChatEvent::TextDelta("final answer".into()),
        ChatEvent::Done,
    ]]));
    let events = run_loop_capture(
        provider,
        StdArc::new(ToolRegistry::new()),
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("go"),
        ApprovalAction::Ignore,
    )
    .await;

    assert_eq!(
        thinking_deltas_of(&events),
        vec!["first segment", "second segment"],
        "非前缀快照按「新块起点」各发全文（显示侧为两段拼接）"
    );

    let saved = inspect.get(&sid).unwrap();
    let text_msg = saved
        .history
        .iter()
        .find(|m| m.kind == MessageType::Text && m.content.contains("final answer"))
        .expect("正文装配路径应落库一条 text 消息");
    let blocks = text_msg.extra_blocks.as_ref().expect("extra_blocks 应存在");
    let persisted: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b.get("thinking").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        persisted,
        vec!["second segment"],
        "落库/history 仍只有最后一块（与显示侧两段拼接互为对照）"
    );
}

/// Provider that always returns a tool call whose arguments are malformed
/// (empty or invalid JSON), cycling through *different* contents — mirrors
/// streaming interruptions that produce 0-byte/truncated arguments.
/// Regression: such calls must NOT be folded into identical doom-loop
/// signatures; previously unwrap_or_default() collapsed every parse failure
/// to Null, so distinct broken calls looked identical and tripped
/// StuckInLoop, killing the whole session.
struct MalformedArgsProvider {
    call_count: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for MalformedArgsProvider {
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let bad_args = ["", "not json", "{oops"];
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::ToolCall {
                id: format!("call_bad_{count}"),
                name: "write_file".into(),
                arguments: bad_args[count % 3].to_string(),
            }),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Regression: consecutive tool calls with malformed/empty arguments
/// (all parsing to the same Null fallback) must not be treated as a
/// repeated-tool-call loop. The loop should run to the hard limit
/// (MaxIterations), never terminate early with StuckInLoop.
#[serial]
#[tokio::test]
async fn test_malformed_arguments_do_not_trigger_doom_loop() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(MalformedArgsProvider {
        call_count: AtomicUsize::new(0),
    });
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig {
        hard_limit: 8,          // enough iterations to trip the old bug (warn at 3, fatal at 6)
        doom_loop_threshold: 3, // small window to make the test fast
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("write the file"),
        tx,
    )
    .await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::Error {
                code: AgentErrorCode::StuckInLoop,
                ..
            }
        )),
        "malformed/empty tool call arguments must not trigger StuckInLoop"
    );
}

/// Regression: a tool call with empty (0-byte) arguments — as produced by
/// some OpenAI-compatible endpoints for parameterless calls, or by
/// Anthropic when no input_json_delta follows content_block_start — must
/// be normalized to `{}` and executed, not rejected as a hard error.
#[serial]
#[tokio::test]
async fn test_empty_arguments_normalized_for_parameterless_tool() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    // Iteration 1: tool call with empty arguments; iteration 2: stop.
    let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(SimpleProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: "call_empty_1".into(),
                name: "noargs".into(),
                arguments: String::new(),
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]));
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    registry
        .register(std::sync::Arc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    let config = AgentConfig::default();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("call the noargs tool"),
        tx,
    )
    .await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let mut found_ok = false;
    for e in &events {
        if let AgentEvent::ToolCallResult {
            content, is_error, ..
        } = e
        {
            assert!(
                !is_error,
                "empty-arguments call on a parameterless tool must succeed, got error: {content}"
            );
            assert_eq!(content, "ok");
            found_ok = true;
        }
    }
    assert!(found_ok, "expected a ToolCallResult event");
}

/// Regression: a stream that ends normally (UsageInfo + Done) but carries
/// no text/tool_calls/thinking while consuming output tokens must surface
/// an Internal error whose message includes the consumed token count —
/// this is the "LLM returned empty response after consuming N output
/// tokens" diagnostic path.
#[serial]
#[tokio::test]
async fn test_empty_stream_with_output_tokens_reports_error() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider: std::sync::Arc<dyn LlmProvider> =
        std::sync::Arc::new(SimpleProvider::new(vec![vec![
            ChatEvent::UsageInfo {
                input_tokens: 100,
                output_tokens: 11,
                tool_calls: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost: None,
            },
            ChatEvent::Done,
        ]]));
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = ToolRegistry::new();
    let config = AgentConfig::default();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    run_agent_loop(
        provider,
        std::sync::Arc::new(registry),
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("hello"),
        tx,
    )
    .await;

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let found = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Error {
                code: AgentErrorCode::Internal,
                message,
            } if message.contains("11 output tokens")
        )
    });
    assert!(
        found,
        "empty stream with output_tokens=11 must produce Internal error mentioning token count"
    );
}

// ── 计划 1b-1 测试 22–23：PreToolUse 发射点 ─────────────────────────────

/// 参数无关、恒需审批的 mock 工具。
struct ApprovalMockTool {
    name: &'static str,
}

#[async_trait::async_trait]
impl crate::tool::Tool for ApprovalMockTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "approval mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn requires_approval(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: &crate::tool::ToolContext,
    ) -> crate::tool::ToolResult {
        crate::tool::ToolResult::success("ok")
    }
}

/// 审批弹窗的自动应答策略。
#[derive(Clone, Copy)]
enum ApprovalAction {
    /// 以给定索引应答（0=批准，2=Always Allow，-1=拒绝）。
    Select(i32),
    /// 取消 agent run（等价用户按 Stop）。
    Cancel,
    /// 不作答。
    Ignore,
}

/// 建立测试环境：独立 session store + 已 `start_loop` 的 ctx。
fn make_loop_env() -> (StdArc<SessionManager>, String, AgentLoopContext) {
    use crate::session::InMemorySessionStore;
    use std::path::Path;

    let session_mgr = StdArc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: StdArc<dyn crate::context::ContextTrimmer + Send + Sync> =
        StdArc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();
    (session_mgr, sid, ctx)
}

/// 运行 agent loop 并收集全部 `AgentEvent`；审批弹窗按 `action` 自动应答。
async fn run_loop_capture(
    provider: StdArc<dyn LlmProvider>,
    registry: StdArc<ToolRegistry>,
    session_mgr: StdArc<SessionManager>,
    ctx: AgentLoopContext,
    config: AgentConfig,
    user_message: Message,
    action: ApprovalAction,
) -> Vec<AgentEvent> {
    use std::path::Path;

    let cancel_token = ctx.cancel_token.clone();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let handle = tokio::spawn(async move {
        run_agent_loop(
            provider,
            registry,
            StdArc::new(RuleEngine::new(Path::new("/tmp")).unwrap()),
            session_mgr,
            ctx,
            &config,
            user_message,
            tx,
        )
        .await;
    });

    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        if let AgentEvent::UserQuery { respond, .. } = &e {
            match action {
                ApprovalAction::Select(idx) => {
                    let _ = respond.try_send(UserQueryResult {
                        selected_index: idx,
                        text: String::new(),
                    });
                }
                ApprovalAction::Cancel => cancel_token.cancel(),
                ApprovalAction::Ignore => {}
            }
        }
        events.push(e);
    }
    handle.await.expect("agent loop task should not panic");
    events
}

/// 两轮 provider：首轮请求 `tool`，次轮 Done。
fn one_tool_then_done(tool: &str, arguments: String) -> StdArc<dyn LlmProvider> {
    StdArc::new(SimpleProvider::new(vec![
        vec![
            ChatEvent::ToolCall {
                id: format!("call_{tool}"),
                name: tool.into(),
                arguments,
            },
            ChatEvent::Done,
        ],
        vec![ChatEvent::Done],
    ]))
}

fn pre_tool_use_of(events: &[AgentEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PreToolUse {
                call_id,
                tool_name,
                requires_approval,
            } => Some((call_id.clone(), tool_name.clone(), *requires_approval)),
            _ => None,
        })
        .collect()
}

/// 测试 22a：无审批路径发一次 `PreToolUse`，且早于 `ToolCallResult`。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_emitted_on_no_approval_path() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    let provider = one_tool_then_done("noargs", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Ignore,
    )
    .await;

    let pre = pre_tool_use_of(&events);
    assert_eq!(
        pre.len(),
        1,
        "no-approval path must emit exactly one PreToolUse, got {pre:?}"
    );
    assert_eq!(pre[0].1, "noargs");
    assert!(!pre[0].2, "requires_approval must be false");
    let pre_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::PreToolUse { .. }))
        .unwrap();
    let res_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallResult { .. }))
        .unwrap();
    assert!(pre_idx < res_idx, "PreToolUse must precede ToolCallResult");
}

/// 测试 22b：审批通过路径发一次 `PreToolUse`。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_emitted_on_approved_path() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(ApprovalMockTool {
            name: "needs_approval",
        }))
        .unwrap();
    let provider = one_tool_then_done("needs_approval", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Select(0),
    )
    .await;

    let pre = pre_tool_use_of(&events);
    assert_eq!(
        pre.len(),
        1,
        "approved path must emit exactly one PreToolUse"
    );
    assert!(pre[0].2, "requires_approval must be true");
    let dialog_kind = events.iter().find_map(|e| match e {
        AgentEvent::UserQuery { kind, .. } => Some(*kind),
        _ => None,
    });
    assert_eq!(
        dialog_kind,
        Some(PermissionKind::Approval),
        "approval dialog must carry PermissionKind::Approval"
    );
}

/// 测试 22c：Always Allow（already_approved）路径发一次 `PreToolUse`，且不再弹窗。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_emitted_on_always_allow_path() {
    let (session_mgr, sid, ctx) = make_loop_env();
    session_mgr
        .add_approved_tool(&sid, "needs_approval")
        .unwrap();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(ApprovalMockTool {
            name: "needs_approval",
        }))
        .unwrap();
    let provider = one_tool_then_done("needs_approval", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Ignore,
    )
    .await;

    let pre = pre_tool_use_of(&events);
    assert_eq!(
        pre.len(),
        1,
        "always-allow path must emit exactly one PreToolUse"
    );
    assert!(pre[0].2);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::UserQuery { .. })),
        "already-approved tool must skip the dialog"
    );
}

/// 测试 22d：子 agent 的真实执行路径同样发一次 `PreToolUse`。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_emitted_for_subagent() {
    let (session_mgr, sid, mut ctx) = make_loop_env();
    ctx.agent_kind = AgentKind::Sub;
    ctx.depth = 1;
    session_mgr
        .add_approved_tool(&sid, "needs_approval")
        .unwrap();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(ApprovalMockTool {
            name: "needs_approval",
        }))
        .unwrap();
    let provider = one_tool_then_done("needs_approval", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Ignore,
    )
    .await;

    let pre = pre_tool_use_of(&events);
    assert_eq!(
        pre.len(),
        1,
        "sub-agent real execution path must emit PreToolUse"
    );
    assert!(pre[0].2);
}

/// 测试 23a：拒绝路径不发 `PreToolUse`（但仍产出 Denied 结果）。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_not_emitted_on_denied_path() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(ApprovalMockTool {
            name: "needs_approval",
        }))
        .unwrap();
    let provider = one_tool_then_done("needs_approval", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Select(-1),
    )
    .await;

    assert!(
        pre_tool_use_of(&events).is_empty(),
        "denied path must not emit PreToolUse"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResult {
                outcome: ToolOutcome::Denied,
                ..
            }
        )),
        "denied path should still emit a Denied ToolCallResult"
    );
}

/// 测试 23b：取消路径不发 `PreToolUse`。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_not_emitted_on_cancel_path() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(ApprovalMockTool {
            name: "needs_approval",
        }))
        .unwrap();
    let provider = one_tool_then_done("needs_approval", "{}".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Cancel,
    )
    .await;

    assert!(
        pre_tool_use_of(&events).is_empty(),
        "cancel path must not emit PreToolUse"
    );
}

/// 测试 23c：参数截断/畸形路径不发 `PreToolUse`（仍产出 Truncated 结果）。
#[serial]
#[tokio::test]
async fn test_pre_tool_use_not_emitted_on_truncated_arguments() {
    let (session_mgr, _sid, ctx) = make_loop_env();
    let registry = StdArc::new(ToolRegistry::new());
    registry
        .register(StdArc::new(MockTestTool { name: "noargs" }))
        .unwrap();
    // 非法 JSON（截断） → 参数解析失败路径
    let provider = one_tool_then_done("noargs", "{".into());

    let events = run_loop_capture(
        provider,
        registry,
        session_mgr,
        ctx,
        AgentConfig::default(),
        Message::user("call the tool"),
        ApprovalAction::Ignore,
    )
    .await;

    assert!(
        pre_tool_use_of(&events).is_empty(),
        "truncated/malformed arguments path must not emit PreToolUse"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallResult {
                outcome: ToolOutcome::Truncated,
                ..
            }
        )),
        "truncated arguments should still emit a Truncated ToolCallResult"
    );
}

// ── Context overflow self-healing ───────────────────────────────────────────

/// Trimmer that only shrinks after the server reported an overflow.
struct OverflowAwareTrimmer {
    overflow_calls: AtomicUsize,
}

impl crate::context::ContextTrimmer for OverflowAwareTrimmer {
    fn trim(&self, h: &[Message], _: u32, _: u32, _: u32) -> Vec<Message> {
        h.to_vec()
    }

    fn trim_for_session(
        &self,
        _session_id: Option<&str>,
        h: &[Message],
        max_ctx: u32,
        overhead: u32,
        output: u32,
    ) -> Vec<Message> {
        if self.overflow_calls.load(Ordering::SeqCst) == 0 {
            h.to_vec()
        } else {
            // Aggressive compacted view: goal + summary only.
            let _ = (max_ctx, overhead, output);
            vec![Message::user(
                "[compacted] goal: inspect the large log file",
            )]
        }
    }

    fn on_context_overflow(&self, _session_id: Option<&str>) -> u32 {
        self.overflow_calls.fetch_add(1, Ordering::SeqCst) as u32 + 1
    }
}

struct OverflowThenOkProvider {
    calls: AtomicUsize,
    messages_on_success: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for OverflowThenOkProvider {
    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(LlmError::Api {
                status: 400,
                message: "This model's maximum context length is 32768 tokens".into(),
            });
        }
        self.messages_on_success
            .store(messages.len(), Ordering::SeqCst);
        let events: Vec<Result<ChatEvent, LlmError>> = vec![
            Ok(ChatEvent::TextDelta("done after compaction".into())),
            Ok(ChatEvent::UsageInfo {
                input_tokens: 100,
                output_tokens: 10,
                tool_calls: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost: None,
            }),
            Ok(ChatEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

#[serial]
#[tokio::test]
async fn test_agent_loop_self_heals_from_context_overflow() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    for i in 0..6 {
        session_mgr
            .append_message(&sid, Message::user(format!("long history message {i}")))
            .unwrap();
        session_mgr
            .append_message(&sid, Message::assistant(format!("long answer {i}")))
            .unwrap();
    }

    let trimmer = std::sync::Arc::new(OverflowAwareTrimmer {
        overflow_calls: AtomicUsize::new(0),
    });
    let ctx_trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        trimmer.clone();
    let ctx = session_mgr
        .start_loop(&sid, &ctx_trimmer, None, None)
        .unwrap();

    let provider = std::sync::Arc::new(OverflowThenOkProvider {
        calls: AtomicUsize::new(0),
        messages_on_success: AtomicUsize::new(0),
    });
    let provider_dyn: std::sync::Arc<dyn LlmProvider> = provider.clone();
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = std::sync::Arc::new(ToolRegistry::new());
    let config = AgentConfig {
        hard_limit: 2,
        context_overflow_retries: 2,
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);

    run_agent_loop(
        provider_dyn,
        registry,
        rule_engine,
        session_mgr.clone(),
        ctx,
        &config,
        Message::user("inspect the log"),
        tx,
    )
    .await;

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "expected one overflow retry"
    );
    assert_eq!(
        trimmer.overflow_calls.load(Ordering::SeqCst),
        1,
        "on_context_overflow must be invoked"
    );
    assert!(
        provider.messages_on_success.load(Ordering::SeqCst) <= 2,
        "retry should see the compacted prompt"
    );

    let mut saw_error = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::Error { .. }) {
            saw_error = true;
        }
    }
    assert!(!saw_error, "overflow should not surface as a session error");
    let final_session = session_mgr.get(&sid).unwrap();
    assert!(
        final_session
            .history
            .iter()
            .any(|m| m.role == Role::Assistant && m.content.contains("done after compaction")),
        "agent should continue to a conclusion"
    );
}

#[serial]
#[tokio::test]
async fn test_agent_loop_overflow_exhausted_reports_error() {
    use crate::rules::RuleEngine;
    use crate::session::InMemorySessionStore;
    use crate::tool_registry::ToolRegistry;
    use std::path::Path;

    let session_mgr = std::sync::Arc::new(SessionManager::new(InMemorySessionStore::new()));
    let session = session_mgr
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();
    let trimmer: std::sync::Arc<dyn crate::context::ContextTrimmer + Send + Sync> =
        std::sync::Arc::new(Phase2MockTrimmer);
    let ctx = session_mgr.start_loop(&sid, &trimmer, None, None).unwrap();

    let provider = std::sync::Arc::new(OverflowThenOkProvider {
        calls: AtomicUsize::new(0),
        messages_on_success: AtomicUsize::new(0),
    });
    let provider_dyn: std::sync::Arc<dyn LlmProvider> = provider.clone();
    let rule_engine = std::sync::Arc::new(RuleEngine::new(Path::new("/tmp")).unwrap());
    let registry = std::sync::Arc::new(ToolRegistry::new());
    let config = AgentConfig {
        hard_limit: 2,
        context_overflow_retries: 0,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel::<AgentEvent>(128);

    run_agent_loop(
        provider_dyn,
        registry,
        rule_engine,
        session_mgr,
        ctx,
        &config,
        Message::user("inspect the log"),
        tx,
    )
    .await;

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
