//! 独立验证环节的编排层测试。
//!
//! 数据形态贴近真实任务：一条用户消息 + 多轮工具调用；主 agent 与验证者使用
//! 不同的模型 key / provider 实例，验证者只看到原始需求与改动信息。

use super::*;

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;

use visp_core::agent::{AgentEvent, VerificationStage};
use visp_core::agent_definition::{AgentDefinition, AgentMode};
use visp_core::bus::BusEvent;
use visp_core::context::NoopTrimmer;
use visp_core::message::{Message, Role, ToolDefinition};
use visp_core::provider::{ChatEvent, LlmConfig, LlmProvider};
use visp_core::rules::RuleEngine;
use visp_core::session::{InMemorySessionStore, SessionManager};
use visp_core::tool::{Tool, ToolContext, ToolResult};
use visp_core::tool_registry::ToolRegistry;

use crate::orchestrator::{CancelSignal, ClientMessage, Orchestrator};
use crate::verifier::VerifierConfig;

const ORIGINAL_REQUEST: &str =
    "请实现核心场景：用户上传 CSV 后按列名查询；真实使用形态是 10 万行文件，不是内存小样本。";

/// 可脚本化的 provider：每次 `chat_stream` 消费一个阶段，并记录收到的消息。
struct ScriptedProvider {
    phases: StdMutex<VecDeque<Vec<ChatEvent>>>,
    calls: AtomicUsize,
    seen: StdMutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(phases: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            phases: StdMutex::new(phases.into()),
            calls: AtomicUsize::new(0),
            seen: StdMutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen_calls(&self) -> Vec<Vec<Message>> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
        _cancel: &CancellationToken,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<ChatEvent, visp_core::error::LlmError>> + Send>,
        >,
        visp_core::error::LlmError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(messages.to_vec());
        let phase = self
            .phases
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![ChatEvent::Done]);
        let events: Vec<Result<ChatEvent, _>> = phase.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }
}

/// 永远成功的工具；名称决定其是否被计为「修改项目文件」。
struct OkTool {
    name: &'static str,
}

#[async_trait]
impl Tool for OkTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
}

fn model(name: &str) -> visp_config::LlmModelConfig {
    visp_config::LlmModelConfig {
        name: name.to_string(),
        protocol: "openai".to_string(),
        provider: Some("Mock".to_string()),
        model: name.to_string(),
        api_key: None,
        base_url: None,
        temperature: None,
        max_tokens: None,
        max_context_tokens: None,
        thinking_budget_tokens: None,
        use_tool: None,
        image_generation: None,
        extra: HashMap::new(),
    }
}

struct VerifyHarness {
    client_tx: mpsc::Sender<ClientMessage>,
    cancel_tx: mpsc::Sender<CancelSignal>,
    recorder: Arc<RecordingPublisher>,
    session_id: String,
    main: Arc<ScriptedProvider>,
    verifier: Arc<ScriptedProvider>,
    _handle: tokio::task::JoinHandle<()>,
}

fn build_harness(
    main_phases: Vec<Vec<ChatEvent>>,
    verifier_phases: Vec<Vec<ChatEvent>>,
    verifier_config: VerifierConfig,
) -> VerifyHarness {
    let (cancel_tx, cancel_rx) = mpsc::channel(4);
    let (global_tx, global_rx) = mpsc::channel(256);
    let (client_tx, client_rx) = mpsc::channel(16);

    let store: Box<dyn visp_core::session::SessionStore> = Box::new(InMemorySessionStore::new());
    let session_mgr = Arc::new(SessionManager::new(store));
    let session = session_mgr
        .create(
            &PathBuf::from("/tmp/visp-verifier-tests"),
            LlmConfig::default(),
        )
        .unwrap();
    let session_id = session.id.clone();

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
        .ok();
    let agent_registry = Arc::new(ArcSwap::from_pointee(registry));

    let tool_registry = Arc::new(ToolRegistry::new());
    for name in ["read_file", "write_file", "edit_file"] {
        tool_registry.register(Arc::new(OkTool { name })).unwrap();
    }

    let main = Arc::new(ScriptedProvider::new(main_phases));
    let verifier = Arc::new(ScriptedProvider::new(verifier_phases));
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    providers.insert("Mock/main".to_string(), main.clone());
    providers.insert("Mock/verifier".to_string(), verifier.clone());

    let daemon_config = Arc::new(visp_config::DaemonConfig {
        llm: visp_config::LlmSection {
            models: vec![model("main"), model("verifier")],
            ..Default::default()
        },
        ..Default::default()
    });

    let recorder = recording_publisher();
    let orch = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx,
        client_rx,
        session_mgr,
        agent_registry,
        tool_registry,
        Arc::new(RuleEngine::new(&PathBuf::from(".")).unwrap()),
        AgentConfig::default(),
        Arc::new(NoopTrimmer),
        daemon_config,
        providers,
        recorder.clone(),
    )
    .with_verifier_config(verifier_config);

    let handle = tokio::spawn(async move {
        let mut orch = orch;
        orch.run().await;
    });

    VerifyHarness {
        client_tx,
        cancel_tx,
        recorder,
        session_id,
        main,
        verifier,
        _handle: handle,
    }
}

impl VerifyHarness {
    async fn submit(&self) {
        self.client_tx
            .send(ClientMessage::UserInput {
                session_id: self.session_id.clone(),
                text: ORIGINAL_REQUEST.to_string(),
            })
            .await
            .unwrap();
    }

    async fn stop(&self) {
        let _ = self.cancel_tx.send(CancelSignal).await;
    }

    fn events(&self) -> Vec<BusEvent> {
        self.recorder.events.lock().unwrap().clone()
    }

    fn verifications(&self) -> Vec<visp_core::agent::VerificationUpdate> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                BusEvent::Frame(f) => match f.event {
                    AgentEvent::Verification(v) => Some(v),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn final_verification(&self) -> Option<visp_core::agent::VerificationUpdate> {
        self.verifications().into_iter().find(|v| v.is_final)
    }

    fn root_done_count(&self) -> usize {
        self.events()
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    BusEvent::Frame(f)
                        if f.session_id == self.session_id
                            && matches!(f.event, AgentEvent::Done)
                )
            })
            .count()
    }

    async fn wait_finished(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.final_verification().is_some() && self.root_done_count() > 0 {
                    return;
                }
                // 无验证场景：只等 root Done。
                if self.verifications().is_empty() && self.root_done_count() > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    async fn wait_root_done(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.root_done_count() > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }
}

fn done_frame(agent: &str, session: &str, parent: Option<&str>) -> Vec<BusEvent> {
    vec![BusEvent::Frame(visp_core::agent::AgentEventFrame {
        event: AgentEvent::Done,
        session_id: session.to_string(),
        agent_name: agent.to_string(),
        parent_session_id: parent.map(str::to_string),
        parent_session_name: None,
    })]
}

fn text(s: &str) -> ChatEvent {
    ChatEvent::TextDelta(s.to_string())
}

fn tool(id: &str, name: &str, args: &str) -> ChatEvent {
    ChatEvent::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
    }
}

fn verification_tool(passed: bool, summary: &str) -> ChatEvent {
    let args = serde_json::json!({
        "passed": passed,
        "summary": summary,
        "evidence": [{
            "check": "核心场景：10 万行 CSV 按列名查询",
            "method": "构造大文件并实际运行查询",
            "result": if passed { "通过，返回正确行数" } else { "失败：仍按全量加载内存实现" }
        }]
    });
    tool("verify-submit", "submit_verification", &args.to_string())
}

// ── 场景 1：只读回合不触发验证 ─────────────────────────────────────────

#[tokio::test]
async fn test_turn_without_file_changes_skips_verification() {
    let harness = build_harness(
        vec![
            vec![
                tool("c1", "read_file", r#"{"path":"src/lib.rs"}"#),
                ChatEvent::Done,
            ],
            vec![text("已完成分析"), ChatEvent::Done],
        ],
        vec![],
        VerifierConfig {
            enabled: true,
            max_rounds: 2,
            model: Some("Mock/verifier".to_string()),
            timeout_secs: 5,
            max_steps: 8,
        },
    );

    harness.submit().await;
    assert!(harness.wait_root_done().await, "只读回合应在超时前结束");

    assert_eq!(harness.verifier.calls(), 0, "未修改文件时不得调用验证者");
    assert!(
        harness.verifications().is_empty(),
        "未修改文件时不应发布任何验证事件"
    );
    harness.stop().await;
}

// ── 场景 2：修改文件后自动验证，且验证者与实现历史隔离 ─────────────────

#[tokio::test]
async fn test_file_change_runs_isolated_verifier_and_passes() {
    let harness = build_harness(
        vec![
            vec![
                tool("c1", "read_file", r#"{"path":"src/lib.rs"}"#),
                ChatEvent::Done,
            ],
            vec![
                tool(
                    "c2",
                    "write_file",
                    r#"{"path":"src/lib.rs","content":"impl"}"#,
                ),
                ChatEvent::Done,
            ],
            vec![text("实现完成"), ChatEvent::Done],
        ],
        vec![
            vec![
                verification_tool(true, "核心场景已实际验证通过"),
                ChatEvent::Done,
            ],
            vec![text("验证完成"), ChatEvent::Done],
        ],
        VerifierConfig {
            enabled: true,
            max_rounds: 2,
            model: Some("Mock/verifier".to_string()),
            timeout_secs: 5,
            max_steps: 8,
        },
    );

    harness.submit().await;
    assert!(harness.wait_finished().await, "验证回合应在超时前结束");

    let final_v = harness.final_verification().expect("必须有最终验证结论");
    assert_eq!(final_v.stage, VerificationStage::Passed);
    assert_eq!(final_v.passed, Some(true));
    assert!(final_v.summary.contains("核心场景"));
    assert!(!final_v.evidence.is_empty(), "结论必须附证据");
    assert_eq!(harness.root_done_count(), 1, "root Done 应恰有一条");

    // 验证者只看到一份全新的上下文：system + 原始需求，不包含实现者的对话/推理。
    let seen = harness.verifier.seen_calls();
    assert!(!seen.is_empty(), "验证者模型应被调用");
    let msgs = &seen[0];
    let user_msgs: Vec<&Message> = msgs.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "验证者只应收到一条用户消息（原始需求+改动）"
    );
    assert!(user_msgs[0].content.contains(ORIGINAL_REQUEST));
    assert!(user_msgs[0].content.contains("src/lib.rs"));
    assert!(
        !user_msgs[0].content.contains("实现完成"),
        "验证者不得拿到实现者的对话历史"
    );
    assert!(
        !msgs.iter().any(|m| m.role == Role::Assistant),
        "验证者上下文不得包含实现者的 assistant 消息"
    );

    // 最终 Done 必须晚于最终验证结论（否则 ACP/headless 会在验证前收尾）。
    let events = harness.events();
    let final_idx = events
        .iter()
        .position(|e| {
            matches!(e, BusEvent::Frame(f)
                if matches!(&f.event, AgentEvent::Verification(v) if v.is_final))
        })
        .unwrap();
    let done_idx = events
        .iter()
        .position(|e| {
            matches!(e, BusEvent::Frame(f)
                if f.session_id == harness.session_id && matches!(f.event, AgentEvent::Done))
        })
        .unwrap();
    assert!(final_idx < done_idx, "最终 Done 必须在验证结论之后");

    harness.stop().await;
}

// ── 场景 3：验证不通过 → 返工 → 再验证通过 ─────────────────────────────

#[tokio::test]
async fn test_failed_verification_reworks_main_agent_then_passes() {
    let harness = build_harness(
        vec![
            vec![
                tool(
                    "c1",
                    "write_file",
                    r#"{"path":"src/query.rs","content":"v1"}"#,
                ),
                ChatEvent::Done,
            ],
            vec![text("初版完成"), ChatEvent::Done],
            // 返工回合
            vec![text("已按验证意见修正"), ChatEvent::Done],
        ],
        vec![
            vec![
                verification_tool(false, "核心场景失败：仍是内存全量加载"),
                ChatEvent::Done,
            ],
            vec![text("验证结束"), ChatEvent::Done],
            vec![
                verification_tool(true, "返工后核心场景通过"),
                ChatEvent::Done,
            ],
            vec![text("验证结束"), ChatEvent::Done],
        ],
        VerifierConfig {
            enabled: true,
            max_rounds: 2,
            model: Some("Mock/verifier".to_string()),
            timeout_secs: 5,
            max_steps: 8,
        },
    );

    harness.submit().await;
    assert!(harness.wait_finished().await, "返工+复验应在超时前结束");

    let vs = harness.verifications();
    assert_eq!(
        vs.iter()
            .filter(|v| v.stage == VerificationStage::Started)
            .count(),
        2,
        "应进行两轮验证"
    );
    assert!(
        vs.iter().any(|v| v.stage == VerificationStage::Rework),
        "验证失败后应发布返工事件"
    );
    let final_v = harness.final_verification().unwrap();
    assert_eq!(final_v.stage, VerificationStage::Passed);
    assert_eq!(final_v.passed, Some(true));
    assert!(
        harness.main.calls() >= 3,
        "主 agent 应执行初版 + 返工，实际 {} 次",
        harness.main.calls()
    );
    let last_main = harness
        .main
        .seen_calls()
        .last()
        .cloned()
        .expect("主 agent 至少被调用一次");
    assert!(
        last_main
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("独立验证未通过")),
        "返工回合必须把验证者的具体问题交回主 agent"
    );
    assert_eq!(harness.root_done_count(), 1);

    harness.stop().await;
}

// ── 场景 4：返工轮数用尽仍未通过 → 明确报告失败，不得当作成功 ─────────

#[tokio::test]
async fn test_verification_exhaustion_reports_failure() {
    let harness = build_harness(
        vec![
            vec![
                tool(
                    "c1",
                    "write_file",
                    r#"{"path":"src/query.rs","content":"bad"}"#,
                ),
                ChatEvent::Done,
            ],
            vec![text("初版完成"), ChatEvent::Done],
            vec![text("返工完成"), ChatEvent::Done],
        ],
        vec![
            vec![
                verification_tool(false, "核心场景仍不满足"),
                ChatEvent::Done,
            ],
            vec![text("验证结束"), ChatEvent::Done],
            vec![
                verification_tool(false, "核心场景仍不满足"),
                ChatEvent::Done,
            ],
            vec![text("验证结束"), ChatEvent::Done],
        ],
        VerifierConfig {
            enabled: true,
            max_rounds: 1,
            model: Some("Mock/verifier".to_string()),
            timeout_secs: 5,
            max_steps: 8,
        },
    );

    harness.submit().await;
    assert!(harness.wait_finished().await, "验证应在超时前收敛");

    let vs = harness.verifications();
    assert_eq!(
        vs.iter()
            .filter(|v| v.stage == VerificationStage::Started)
            .count(),
        2,
        "max_rounds=1 时应验证两次（首验 + 一次复验）"
    );
    assert_eq!(
        vs.iter()
            .filter(|v| v.stage == VerificationStage::Rework)
            .count(),
        1,
        "只允许返工一次"
    );
    let final_v = harness.final_verification().unwrap();
    assert_eq!(final_v.stage, VerificationStage::Exhausted);
    assert_eq!(final_v.passed, Some(false));
    assert!(final_v.summary.contains("仍不满足"));
    assert_eq!(harness.root_done_count(), 1, "失败也必须发出 Done 收尾");
    assert!(
        vs.iter().any(|v| v.stage == VerificationStage::Exhausted),
        "达到上限应显式发布 exhausted 事件"
    );

    harness.stop().await;
}

// ── 场景 5：开关关闭时完全不触发 ───────────────────────────────────────

#[tokio::test]
async fn test_disabled_verifier_skips_even_with_file_changes() {
    let harness = build_harness(
        vec![
            vec![
                tool("c1", "write_file", r#"{"path":"src/lib.rs","content":"x"}"#),
                ChatEvent::Done,
            ],
            vec![text("完成"), ChatEvent::Done],
        ],
        vec![],
        VerifierConfig::disabled(),
    );

    harness.submit().await;
    assert!(harness.wait_root_done().await);
    assert_eq!(harness.verifier.calls(), 0);
    assert!(harness.verifications().is_empty());
    harness.stop().await;
}

// ── 场景 6：子 agent 修改文件也必须触发根回合验证 ───────────────────────

#[test]
fn test_child_agent_file_change_maps_to_root_ancestor() {
    let mut registry = ActiveAgentRegistry::new();
    let (tx, _rx) = mpsc::channel(1);
    registry.register(ActiveAgent {
        session_id: "root".into(),
        parent_session_id: None,
        agent_name: "default".into(),
        cancel_token: Default::default(),
        inbox: tx.clone(),
        pending_call_id: None,
        started_at: std::time::Instant::now(),
    });
    registry.register(ActiveAgent {
        session_id: "child".into(),
        parent_session_id: Some("root".into()),
        agent_name: "fixer".into(),
        cancel_token: Default::default(),
        inbox: tx,
        pending_call_id: Some("call-1".into()),
        started_at: std::time::Instant::now(),
    });

    assert_eq!(root_ancestor(&registry, "child"), "root");
    assert_eq!(root_ancestor(&registry, "root"), "root");
}

// 保持汇总函数存在，避免 `done_frame` 未使用（测试辅助预留）。
#[test]
fn test_done_frame_helper_compiles() {
    assert_eq!(done_frame("default", "s", None).len(), 1);
}

// ── 场景 7：验证者没给出结论 ≠ 发现缺陷，不得触发返工 ─────────────────

#[tokio::test]
async fn test_verifier_without_conclusion_does_not_rework_main_agent() {
    // 验证者只有文本、没有 submit_verification，也没有 VERDICT 标记：
    // 这表示「验证没做成」，不是「代码有缺陷」。主 agent 不应被要求返工。
    let harness = build_harness(
        vec![
            vec![
                tool(
                    "c1",
                    "write_file",
                    r#"{"path":"src/query.rs","content":"v1"}"#,
                ),
                ChatEvent::Done,
            ],
            vec![text("初版完成"), ChatEvent::Done],
            // 如果发生返工，这个阶段会被消费；断言主 agent 不应被再次调用。
            vec![text("不应出现的返工"), ChatEvent::Done],
        ],
        vec![
            vec![text("我看了一下，但还没有运行任何场景"), ChatEvent::Done],
            vec![text("第二轮也不给结论"), ChatEvent::Done],
        ],
        VerifierConfig {
            enabled: true,
            max_rounds: 2,
            model: Some("Mock/verifier".to_string()),
            timeout_secs: 5,
            max_steps: 1,
        },
    );

    harness.submit().await;
    assert!(harness.wait_finished().await, "验证应在超时前收敛");

    let vs = harness.verifications();
    assert_eq!(
        vs.iter()
            .filter(|v| v.stage == VerificationStage::Started)
            .count(),
        1,
        "验证者没给出结论时不应自动开新一轮验证"
    );
    assert!(
        !vs.iter().any(|v| v.stage == VerificationStage::Rework),
        "没给出结论不得触发返工"
    );
    let final_v = harness.final_verification().unwrap();
    assert_eq!(final_v.passed, Some(false), "没做成也绝不能当作通过");
    assert!(
        final_v.summary.contains("未提交结构化结论"),
        "最终结论应说明验证没做成，而不是声称发现缺陷：{}",
        final_v.summary
    );
    // 初版本身包含两次模型调用（写文件 + 收尾文本）；没有返工就不会有第三次。
    assert_eq!(
        harness.main.calls(),
        2,
        "主 agent 只应执行初版，不应因验证没做成而返工"
    );
    assert!(
        !harness.main.seen_calls().iter().any(|msgs| msgs
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("独立验证未通过"))),
        "没有具体缺陷时不得向主 agent 发送返工提示"
    );
    assert_eq!(harness.root_done_count(), 1, "仍须正常 Done 收尾");

    harness.stop().await;
}
