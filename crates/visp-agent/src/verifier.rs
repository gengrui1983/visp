//! 独立验证者模块
//!
//! 验证者运行在一个全新的子会话中：它没有实现者的对话历史，只拿到原始用户请求、
//! 本轮改动文件列表和 git diff 快照。它必须自己设计验收场景并实际执行，最后通过
//! `submit_verification` 工具提交结构化结论（passed / summary / evidence）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing;

use visp_core::agent::run_agent_loop;
use visp_core::agent::{AgentConfig, AgentEvent, AgentEventFrame, AgentKind, VerificationEvidence};
use visp_core::bus::{BusEvent, EventPublisher};
use visp_core::context::ContextTrimmer;
use visp_core::message::{Message, Role};
use visp_core::provider::LlmProvider;
use visp_core::rules::RuleEngine;
use visp_core::session::{SessionManager, SessionStatus, SubSessionParams};
use visp_core::tool::{Tool, ToolContext, ToolResult};
use visp_core::tool_registry::ToolRegistry;

/// 验证者在会话与事件帧中使用的 agent 名称。
pub const VERIFIER_AGENT_NAME: &str = "verifier";

/// 验证者提交最终结论的专用工具名。
pub const SUBMIT_VERIFICATION_TOOL: &str = "submit_verification";

/// 验证者配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierConfig {
    pub enabled: bool,
    /// 首验失败后允许的返工轮数
    pub max_rounds: u32,
    /// 模型 key（"{provider}/{name}"）；None = 继承主会话模型
    pub model: Option<String>,
    pub timeout_secs: u64,
    pub max_steps: u32,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 2,
            model: None,
            timeout_secs: 600,
            max_steps: 40,
        }
    }
}

impl VerifierConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// 一次验证的最终报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub passed: bool,
    /// 验证者是否真正完成了验证并给出结论。
    ///
    /// `false` 表示「验证没做成」（超时、步数用尽、未提交结论等），而不是
    /// 「验证发现代码有缺陷」。两者都必须判为未通过，但后续处理不同：
    /// 只有 `completed = true && passed = false` 才触发主 agent 返工。
    pub completed: bool,
    pub summary: String,
    pub evidence: Vec<VerificationEvidence>,
}

impl VerificationReport {
    /// 构造「验证没做成」报告（无证据，不触发返工）。
    pub fn failure(summary: impl Into<String>) -> Self {
        Self {
            passed: false,
            completed: false,
            summary: summary.into(),
            evidence: Vec::new(),
        }
    }
}

/// 提交结果的共享槽。
///
/// `submit_verification` 工具写入，`run_verifier` 在子会话结束后读取。
#[derive(Clone, Default)]
pub struct VerdictCell(Arc<Mutex<Option<VerificationReport>>>);

impl VerdictCell {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取出结论（只能取一次）。
    pub fn take(&self) -> Option<VerificationReport> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// 验证者专用工具：提交结构化验证结论。
pub struct SubmitVerificationTool {
    cell: VerdictCell,
}

impl SubmitVerificationTool {
    pub fn new(cell: VerdictCell) -> Self {
        Self { cell }
    }
}

#[async_trait]
impl Tool for SubmitVerificationTool {
    fn name(&self) -> &str {
        SUBMIT_VERIFICATION_TOOL
    }

    fn description(&self) -> &str {
        "提交独立验证的最终结论；必须恰好调用一次。必须附带至少一条证据，\
         说明检查了什么、怎么检查、看到的结果。"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "passed": {
                    "type": "boolean",
                    "description": "验证是否通过"
                },
                "summary": {
                    "type": "string",
                    "description": "验证结论摘要"
                },
                "evidence": {
                    "type": "array",
                    "description": "证据列表，至少一条",
                    "items": {
                        "type": "object",
                        "required": ["check", "method", "result"],
                        "properties": {
                            "check": { "type": "string", "description": "检查了什么" },
                            "method": { "type": "string", "description": "怎么检查" },
                            "result": { "type": "string", "description": "看到的结果" }
                        }
                    }
                }
            },
            "required": ["passed", "summary", "evidence"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value, _context: &ToolContext) -> ToolResult {
        let Some(passed) = arguments.get("passed").and_then(|v| v.as_bool()) else {
            return ToolResult::error("参数错误：passed 必须是布尔值");
        };

        let summary = arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        if summary.is_empty() {
            return ToolResult::error("参数错误：summary 不能为空");
        }

        let Some(evidence_values) = arguments.get("evidence").and_then(|v| v.as_array()) else {
            return ToolResult::error("参数错误：evidence 必须是数组");
        };
        if evidence_values.is_empty() {
            return ToolResult::error("参数错误：evidence 至少需要一条");
        }

        let mut evidence = Vec::with_capacity(evidence_values.len());
        for (index, item) in evidence_values.iter().enumerate() {
            let field = |key: &str| {
                item.get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            };
            let (Some(check), Some(method), Some(result)) =
                (field("check"), field("method"), field("result"))
            else {
                return ToolResult::error(format!(
                    "参数错误：evidence[{index}] 必须包含非空的 check/method/result"
                ));
            };
            evidence.push(VerificationEvidence {
                check: check.to_string(),
                method: method.to_string(),
                result: result.to_string(),
            });
        }

        let report = VerificationReport {
            passed,
            completed: true,
            summary: summary.to_string(),
            evidence,
        };

        match self.cell.0.lock() {
            Ok(mut slot) => {
                if slot.is_some() {
                    return ToolResult::error("verification already submitted");
                }
                *slot = Some(report);
                ToolResult::success("验证结论已提交")
            }
            Err(_) => ToolResult::error("内部错误：验证结论槽不可用"),
        }
    }
}

/// 验证者 system prompt：强调隔离纪律与「必须实际执行」。
pub fn build_verifier_system_prompt() -> String {
    r#"你是独立验证者（verifier）。你没有参与实现，必须保持独立判断。

## 隔离纪律
- 你没有参与实现，不得相信实现者的测试、注释或结论；实现者写的测试不能作为验收依据。
- 唯一需求来源是用户消息中给出的「原始用户请求」。不要自行扩大或缩小需求。
- 你必须自己从原始需求推导验收场景，并**实际执行**：读代码、跑命令、必要时写临时测试文件或临时脚本。
- 不能只重跑实现者写的测试，也不能只读 diff 就给意见。任何未经实际演示的行为一律判 fail。

## 结论要求
- 每条结论都要附证据：检查了什么 / 怎么检查 / 看到的结果。
- 优先覆盖真实使用形态（边界条件、规模、错误输入），而不只是简单路径。
- 完成前必须调用一次 `submit_verification`，提交 passed、summary 和 evidence。
- 只读检查项目；可以创建临时测试文件，但不要修改项目源码来「让验证通过」。"#
        .to_string()
}

/// 构造验证者 user prompt：原始需求 + 改动文件 + 改动快照。
pub fn build_verification_prompt(
    original_request: &str,
    changed_paths: &[String],
    change_context: &str,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("## 原始用户请求（唯一需求来源）\n");
    prompt.push_str(original_request);
    prompt.push_str("\n\n## 本轮改动的文件\n");
    if changed_paths.is_empty() {
        prompt.push_str("(无)\n");
    } else {
        for path in changed_paths {
            prompt.push_str("- ");
            prompt.push_str(path);
            prompt.push('\n');
        }
    }

    prompt.push_str("\n## 改动快照（git diff/status，可能截断）\n");
    if change_context.trim().is_empty() {
        prompt.push_str("(无 git 信息，请直接检查工作区)\n");
    } else {
        prompt.push_str(change_context);
        if !change_context.ends_with('\n') {
            prompt.push('\n');
        }
    }

    prompt.push_str(
        "\n请先复述原始需求隐含的核心验收场景，再逐条实际验证，最后调用 submit_verification。\n",
    );
    prompt
}

/// 捕获改动信息：改动文件列表 + git status/diff 快照。
///
/// git 不可用或命令失败时静默忽略，改动文件列表始终保留。
pub async fn capture_change_context(project_path: &Path, changed_paths: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !changed_paths.is_empty() {
        let mut listing = String::from("改动文件：\n");
        for path in changed_paths {
            listing.push_str("- ");
            listing.push_str(path);
            listing.push('\n');
        }
        parts.push(listing.trim_end().to_string());
    }

    let git_outputs = [
        ("git status --porcelain", vec!["status", "--porcelain"]),
        ("git diff --stat", vec!["diff", "--no-color", "--stat"]),
        ("git diff", vec!["diff", "--no-color"]),
    ];
    for (label, args) in git_outputs {
        if let Some(output) = run_git(project_path, &args).await {
            parts.push(format!("{label}:\n{output}"));
        }
    }

    let mut context = parts.join("\n\n");
    let max_chars = 24_000usize;
    if context.chars().count() > max_chars {
        context = context.chars().take(max_chars).collect();
        context.push_str("\n…（已截断）");
    }
    context
}

/// 执行一条只读 git 命令；失败、非 git 仓库或空输出返回 None。
async fn run_git(project_path: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(project_path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// 会修改项目文件的工具名（大小写不敏感）。
#[allow(dead_code)] // 待 orchestrator 接入改动追踪后使用
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    const MUTATING_TOOLS: [&str; 5] = [
        "write_file",
        "edit_file",
        "multi_edit",
        "apply_patch",
        "notebook_edit",
    ];
    MUTATING_TOOLS
        .iter()
        .any(|mutating| mutating.eq_ignore_ascii_case(name))
}

/// 从工具参数 JSON 中提取 path / file_path；解析失败返回 None。
#[allow(dead_code)] // 待 orchestrator 接入改动追踪后使用
pub(crate) fn extract_changed_path(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let path = value
        .get("path")
        .or_else(|| value.get("file_path"))
        .and_then(|v| v.as_str())?
        .trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// 一次验证运行所需的上下文。
pub struct VerifierRunRequest {
    pub parent_session_id: String,
    /// 验证会话 id（由 orchestrator 预先生成并登记，便于取消/展示）。
    pub session_id: String,
    pub original_request: String,
    pub changed_paths: Vec<String>,
    pub project_path: PathBuf,
    /// 已应用验证者模型覆盖的 LlmConfig（orchestrator 负责解析模型）
    pub base_config: visp_core::provider::LlmConfig,
    /// 从父会话继承的 Always-Allow 工具集
    pub approved_tools: HashSet<String>,
}

/// 运行一次独立的验证者子会话，返回验证结论。
///
/// 所有失败路径都返回明确的 `VerificationReport`，不会 panic。
#[allow(clippy::too_many_arguments)]
pub async fn run_verifier(
    provider: Arc<dyn LlmProvider>,
    tool_registry: Arc<ToolRegistry>,
    rule_engine: Arc<RuleEngine>,
    session_mgr: Arc<SessionManager>,
    context_trimmer: Arc<dyn ContextTrimmer + Send + Sync>,
    bus: Arc<dyn EventPublisher>,
    config: VerifierConfig,
    req: VerifierRunRequest,
) -> VerificationReport {
    // 1. 捕获改动快照
    let change_context = capture_change_context(&req.project_path, &req.changed_paths).await;

    // 2. 复制工具表（剔除 Agent 类工具，验证者不能再委托）并注册提交工具
    let registry = crate::orchestrator::filter_tools_for_sub_agent(&tool_registry, &[]);
    let cell = VerdictCell::new();
    let _ = registry.register(Arc::new(SubmitVerificationTool::new(cell.clone())));

    // 3. 创建全新子会话（无实现者历史）
    let id = req.session_id.clone();
    if let Err(e) = session_mgr.create_sub(SubSessionParams {
        parent_id: Some(req.parent_session_id.clone()),
        agent_name: VERIFIER_AGENT_NAME.into(),
        permission: vec![],
        session_id: Some(id.clone()),
        project_path: req.project_path.clone(),
        config: req.base_config.clone(),
        approved_tools: req.approved_tools.clone(),
    }) {
        return VerificationReport::failure(format!("无法创建验证会话: {e}"));
    }

    // 4. 追加验证者 system prompt
    if let Err(e) = session_mgr.append_system_prompt_template(&id, &build_verifier_system_prompt())
    {
        tracing::warn!(session_id = %id, error = %e, "追加验证者 system prompt 失败");
    }

    // 5. 启动循环上下文
    let mut ctx =
        match session_mgr.start_loop(&id, &context_trimmer, None, Some(Arc::new(Vec::new()))) {
            Ok(ctx) => ctx,
            Err(e) => return VerificationReport::failure(format!("无法启动验证会话: {e}")),
        };
    ctx.agent_kind = AgentKind::Sub;
    ctx.depth = 1;
    ctx.parent_session_id = req.parent_session_id.clone();

    // 6. 事件转发：验证者事件以子会话身份发到总线
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let forward_bus = bus.clone();
    let forward_session_id = id.clone();
    let forward_parent_id = req.parent_session_id.clone();
    let forward_handle = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            forward_bus.publish(BusEvent::Frame(AgentEventFrame {
                event,
                session_id: forward_session_id.clone(),
                agent_name: VERIFIER_AGENT_NAME.into(),
                parent_session_id: Some(forward_parent_id.clone()),
                parent_session_name: None,
            }));
        }
    });

    // 7. 步数限制
    let mut ac = AgentConfig::default();
    ac.hard_limit = config.max_steps.max(1);
    ac.soft_limit = (ac.hard_limit * 4 / 5).max(1);

    // 8. 运行验证者循环（超时则取消并结束会话）
    let cancel = ctx.cancel_token.clone();
    let prompt =
        build_verification_prompt(&req.original_request, &req.changed_paths, &change_context);
    let run = run_agent_loop(
        provider,
        registry,
        rule_engine,
        session_mgr.clone(),
        ctx,
        &ac,
        Message::user(prompt),
        tx,
    );
    let run_result =
        tokio::time::timeout(Duration::from_secs(config.timeout_secs.max(1)), run).await;

    if run_result.is_err() {
        cancel.cancel();
        let _ = session_mgr.finish_loop(&id, SessionStatus::Idle);
        // 等转发任务收尾，确保已产生的事件都发布出去
        let _ = forward_handle.await;
        return VerificationReport::failure("验证者超时");
    }
    let _ = forward_handle.await;

    // 9. 优先使用 submit_verification 的结构化结论
    if let Some(report) = cell.take() {
        return report;
    }

    // 10. 兜底：解析最终回答中的 VERDICT 标记
    if let Some(report) = parse_verdict_from_session(&session_mgr, &id) {
        return report;
    }

    VerificationReport::failure(
        "验证者未提交结构化结论（submit_verification 未调用，且最终回答无 VERDICT 标记）",
    )
}

/// 从会话最后一条 assistant 消息中解析 `VERDICT: PASS` / `VERDICT: FAIL`。
fn parse_verdict_from_session(
    session_mgr: &SessionManager,
    session_id: &str,
) -> Option<VerificationReport> {
    let messages = session_mgr.get_messages(session_id).ok()?;
    let content = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)?
        .content
        .clone();
    let upper = content.to_uppercase();
    let marker = "VERDICT:";
    let pos = upper.rfind(marker)?;
    let rest = upper[pos + marker.len()..].trim_start();
    let passed = if rest.starts_with("PASS") {
        true
    } else if rest.starts_with("FAIL") {
        false
    } else {
        return None;
    };

    let result = if passed {
        "发现 VERDICT: PASS"
    } else {
        "发现 VERDICT: FAIL"
    };
    Some(VerificationReport {
        passed,
        completed: true,
        summary: content.trim().to_string(),
        evidence: vec![VerificationEvidence {
            check: "验证者最终回答是否给出明确结论".to_string(),
            method: "解析最终回答中的 VERDICT 标记".to_string(),
            result: result.to_string(),
        }],
    })
}

#[cfg(test)]
#[path = "verifier_tests.rs"]
mod tests;
