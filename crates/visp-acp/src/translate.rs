//! 事件翻译器:visp `ServerMessage` → ACP `Outbound`(纯函数,便于表驱动单测)。
//!
//! 判据纪律(§11 通用原则):归属与终止判定**只用 `session_id`**——
//! 主 agent 事件实际带 `agent_name = "default"` 非空(V9),proto 注释不可信。
//! `UserQuery` 一律旁路归属直接分流(§8.4 特例:审批/提问必须可见)。

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionUpdate, StopReason, TextContent,
    ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use visp_proto::visp::{ServerMessage, UserQuery, server_message};

/// 取消的收尾信号:code 为 Display 文本(V10),双向匹配降低文案漂移风险。
pub const CANCELLED_CODE: &str = "Operation cancelled";

/// 工具输出截断阈值(超长保留首尾与提示,§8)。
const TOOL_OUTPUT_MAX_CHARS: usize = 8000;
const TOOL_OUTPUT_HEAD: usize = 4000;
const TOOL_OUTPUT_TAIL: usize = 2000;

/// 翻译输出:父 loop 事件泵消费的指令。
#[derive(Debug, Clone)]
pub enum Outbound {
    /// 经 ACP `session/update` 通知下发
    Update(Box<SessionUpdate>),
    /// 错误文本(随 refusal 终止前下发,保证错误不静默,§6.4 步骤 6)
    ErrorText(String),
    /// 需要人工介入:工具审批(UserQuery options 为空,§6.5)
    ApprovalNeeded(UserQuery),
    /// 需要人工介入:LLM 提问(UserQuery options 非空,§6.4 步骤 7)
    QuestionNeeded(UserQuery),
    /// turn 终止(正常 Done / 取消 / 异常)
    FinishTurn(StopReason),
}

/// 本 turn 的最终验证状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Passed,
    /// 验证者完成了验证并发现了缺陷（或返工轮数用尽）。
    Failed,
    /// 验证者没做成验证（超时/步数用尽/未提交结论），不是代码缺陷。
    Incomplete,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationStatus::Passed => "passed",
            VerificationStatus::Failed => "failed",
            VerificationStatus::Incomplete => "incomplete",
        }
    }
}

/// 翻译上下文:归属主判据 + messageId 近似轮换状态(§7.3)。
pub struct TranslateCtx {
    /// 在途父 prompt 的 visp session_id(归属与终止的唯一判据)
    pub parent_session_id: String,
    /// 当前 assistant 消息的 messageId(父 turn 内稳定)
    message_id: MessageId,
    message_seq: u64,
    /// 上一个文本事件的来源 agent(变化 → 轮换 messageId)
    last_agent: Option<String>,
    /// 本 turn 最终验证结论(None = 未触发验证)。
    verification: Option<VerificationStatus>,
}

impl TranslateCtx {
    pub fn new(parent_session_id: impl Into<String>) -> Self {
        Self {
            parent_session_id: parent_session_id.into(),
            message_id: MessageId::new("msg-0"),
            message_seq: 0,
            last_agent: None,
            verification: None,
        }
    }

    /// 取出最终验证结论(在 Done 收尾时写入 PromptResponse 的 `_meta`)。
    pub fn take_verification(&mut self) -> Option<VerificationStatus> {
        self.verification.take()
    }

    /// 同一 agent 的连续文本保持同一 messageId;切换 agent 时轮换。
    fn ensure_message_id(&mut self, agent_name: &str) -> MessageId {
        if self.last_agent.as_deref() != Some(agent_name) {
            self.message_seq += 1;
            self.message_id = MessageId::new(format!("msg-{}", self.message_seq));
            self.last_agent = Some(agent_name.to_string());
        }
        self.message_id.clone()
    }

    /// 工具帧插入 → 下一段文本换新 messageId(消息边界近似)。
    fn rotate_for_tool(&mut self) {
        self.last_agent = None;
    }
}

/// 翻译一条入站 `ServerMessage`。
pub fn translate(msg: &ServerMessage, ctx: &mut TranslateCtx) -> Vec<Outbound> {
    let Some(payload) = msg.payload.as_ref() else {
        return vec![];
    };
    match payload {
        server_message::Payload::TextDelta(td) => {
            if td.session_id != ctx.parent_session_id {
                return vec![]; // 子 agent 的 TextDelta 在 M1 抑制(§8.4,避免双重展示)
            }
            let mid = ctx.ensure_message_id(&td.agent_name);
            vec![Outbound::Update(Box::new(
                SessionUpdate::AgentMessageChunk(text_chunk(&td.delta, mid)),
            ))]
        }
        server_message::Payload::ThinkingBlock(tb) => {
            if tb.session_id != ctx.parent_session_id {
                return vec![]; // 与 TextDelta 抑制策略对齐(无 agent_name,V14)
            }
            let mid = ctx.ensure_message_id("");
            vec![Outbound::Update(Box::new(
                SessionUpdate::AgentThoughtChunk(text_chunk(&tb.thinking, mid)),
            ))]
        }
        server_message::Payload::ToolCall(tc) => {
            // 子 agent 工具帧刻意照常流式(§8.4:用户能看到子 agent 在做什么)
            ctx.rotate_for_tool();
            vec![Outbound::Update(Box::new(SessionUpdate::ToolCall(
                AcpToolCall::new(
                    ToolCallId::new(tc.call_id.clone()),
                    tool_title(&tc.tool_name, &tc.arguments),
                )
                .kind(tool_kind(&tc.tool_name))
                .status(ToolCallStatus::Pending),
            )))]
        }
        server_message::Payload::ToolResult(tr) => {
            vec![Outbound::Update(Box::new(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(ToolCallId::new(tr.call_id.clone()), tool_result_fields(tr)),
            )))]
        }
        server_message::Payload::UserQuery(q) => {
            // 特例:UserQuery 无 agent_name(V14),一律绕过归属直接桥接——
            // 子 agent 的审批/提问也必须可见,否则 120s 超时判拒或子循环永久阻塞
            if q.options.is_empty() {
                vec![Outbound::ApprovalNeeded(q.clone())]
            } else {
                vec![Outbound::QuestionNeeded(q.clone())]
            }
        }
        server_message::Payload::Done(d) => {
            if d.session_id != ctx.parent_session_id {
                return vec![]; // 子 Done 不终止父 turn
            }
            vec![Outbound::FinishTurn(StopReason::EndTurn)]
        }
        server_message::Payload::Error(e) => {
            if e.session_id != ctx.parent_session_id {
                return vec![]; // 子 agent 的 Error 不得终止父 turn(§6.4 步骤 6)
            }
            // 取消判定双向匹配:code Display 文本 或 message 含 "cancelled"(§6.6)
            if e.code == CANCELLED_CODE || e.message.contains("cancelled") {
                return vec![Outbound::FinishTurn(StopReason::Cancelled)];
            }
            vec![
                Outbound::ErrorText(e.message.clone()),
                Outbound::FinishTurn(StopReason::Refusal),
            ]
        }
        server_message::Payload::VerificationUpdate(v) => {
            // 验证事件归属根会话发布;不按 session_id 抑制(即使在子会话中产生,
            // 验证进度也必须对客户端可见)。最终结论记录到 TranslateCtx,
            // 由 prompt_task 写入 PromptResponse._meta。
            if v.is_final {
                ctx.verification = Some(match v.stage.as_str() {
                    "passed" => VerificationStatus::Passed,
                    "inconclusive" => VerificationStatus::Incomplete,
                    _ => VerificationStatus::Failed,
                });
            }
            let mid = ctx.ensure_message_id(&v.agent_name);
            vec![Outbound::Update(Box::new(
                SessionUpdate::AgentMessageChunk(text_chunk(&format_verification(v), mid)),
            ))]
        }
        server_message::Payload::StatusUpdate(su) => {
            if su.session_id != ctx.parent_session_id {
                return vec![];
            }
            let mid = ctx.ensure_message_id(&su.agent_name);
            vec![Outbound::Update(Box::new(
                SessionUpdate::AgentMessageChunk(text_chunk(&su.message, mid)),
            ))]
        }
        // M1 抑制:UsageInfo/UsageDelta/UserMessage/ImageBlock/ImageError(M2/M3 再启用)
        _ => vec![],
    }
}

// ===== 辅助构造(纯函数) =====

/// 把结构化验证事件渲染为人类可读文本(ACP 无专用 session update 类型)。
fn format_verification(v: &visp_proto::visp::VerificationUpdate) -> String {
    let head = match v.stage.as_str() {
        "started" => format!(
            "🔍 独立验证开始:第 {} 轮(返工上限 {})",
            v.round, v.max_rounds
        ),
        "passed" => format!("✅ 验证通过:{}", v.summary),
        "failed" => {
            if v.is_final {
                format!("❌ 验证未通过:{}", v.summary)
            } else {
                format!("❌ 验证未通过(将继续返工):{}", v.summary)
            }
        }
        "rework" => format!("↩ {}", v.summary),
        "exhausted" => format!(
            "❌ 验证未通过(已用尽 {} 轮返工):{}",
            v.max_rounds, v.summary
        ),
        "inconclusive" => format!("⚠️ 验证未完成(验证没做成，不是发现缺陷):{}", v.summary),
        other => format!("[验证 {}] {}", other, v.summary),
    };
    if v.evidence.is_empty() {
        return head;
    }
    let mut out = head;
    out.push_str("\n证据:");
    for ev in &v.evidence {
        out.push_str(&format!(
            "\n- 检查:{} | 方法:{} | 结果:{}",
            ev.check, ev.method, ev.result
        ));
    }
    out
}

fn text_chunk(text: &str, message_id: MessageId) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text))).message_id(message_id)
}

/// 工具名 → ACP kind(§7.4;visp 工具名以 TODO 登记列表为准)。
pub(crate) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" => ToolKind::Read,
        "grep" | "glob" => ToolKind::Search,
        n if n.starts_with("codegraph_") => ToolKind::Search,
        "write_file" | "edit_file" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        "webfetch" | "fetch_web" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// 人类可读标题:工具名 + 参数摘要(截断)。
fn tool_title(tool_name: &str, arguments: &str) -> String {
    let args: String = arguments.chars().take(80).collect();
    if args.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}({args})")
    }
}

/// 工具结果字段:状态 + 超长输出截断(保留首尾与提示,§8)。
fn tool_result_fields(tr: &visp_proto::visp::ToolResult) -> ToolCallUpdateFields {
    let mut fields = ToolCallUpdateFields::new().status(if tr.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    });
    fields.content = Some(vec![ToolCallContent::from(ContentBlock::Text(
        TextContent::new(truncate_tool_output(&tr.content)),
    ))]);
    fields
}

fn truncate_tool_output(content: &str) -> String {
    let total = content.chars().count();
    if total <= TOOL_OUTPUT_MAX_CHARS {
        return content.to_string();
    }
    let head: String = content.chars().take(TOOL_OUTPUT_HEAD).collect();
    let tail: String = content
        .chars()
        .skip(total - TOOL_OUTPUT_TAIL)
        .take(TOOL_OUTPUT_TAIL)
        .collect();
    format!(
        "{head}\n…[输出过长,已截断 {omitted} 字符]…\n{tail}",
        omitted = total - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use visp_proto::visp::{
        Done, Error as ProtoError, TextDelta, ThinkingBlock, ToolCall as ProtoToolCall,
        ToolResult as ProtoToolResult, UsageInfo, server_message,
    };

    const PARENT: &str = "parent-session";
    const CHILD: &str = "child-session";

    fn ctx() -> TranslateCtx {
        TranslateCtx::new(PARENT)
    }

    fn msg(payload: server_message::Payload) -> ServerMessage {
        ServerMessage {
            payload: Some(payload),
        }
    }

    fn text_delta(sid: &str, agent: &str, delta: &str) -> ServerMessage {
        msg(server_message::Payload::TextDelta(TextDelta {
            delta: delta.into(),
            session_id: sid.into(),
            agent_name: agent.into(),
        }))
    }

    fn thinking(sid: &str, text: &str) -> ServerMessage {
        msg(server_message::Payload::ThinkingBlock(ThinkingBlock {
            thinking: text.into(),
            signature: String::new(),
            session_id: sid.into(),
        }))
    }

    /// 从 translate 输出提取思考 chunk 的 `(messageId, 文本)`;非单帧 thought chunk → panic。
    fn thought_chunk_of(out: &[Outbound]) -> (MessageId, String) {
        assert_eq!(out.len(), 1, "单帧思考应产出恰好一条 Outbound");
        match &out[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::AgentThoughtChunk(chunk) => {
                    let text = match &chunk.content {
                        ContentBlock::Text(t) => t.text.clone(),
                        other => panic!("expected Text content, got {other:?}"),
                    };
                    let mid = chunk
                        .message_id
                        .clone()
                        .expect("思考 chunk 必须带 messageId");
                    (mid, text)
                }
                other => panic!("expected AgentThoughtChunk, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }
    }

    fn tool_call(sid: &str, agent: &str, call_id: &str, name: &str) -> ServerMessage {
        msg(server_message::Payload::ToolCall(ProtoToolCall {
            call_id: call_id.into(),
            tool_name: name.into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
            session_id: sid.into(),
            agent_name: agent.into(),
        }))
    }

    fn tool_result(sid: &str, call_id: &str, content: &str, is_error: bool) -> ServerMessage {
        msg(server_message::Payload::ToolResult(ProtoToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
            session_id: sid.into(),
            tool_name: "bash".into(),
            agent_name: String::new(),
        }))
    }

    fn user_query(options: Vec<String>) -> ServerMessage {
        msg(server_message::Payload::UserQuery(UserQuery {
            query_id: "q-1".into(),
            message: "选择?".into(),
            session_id: PARENT.into(),
            options,
            allow_other: false,
        }))
    }

    fn done(sid: &str) -> ServerMessage {
        msg(server_message::Payload::Done(Done {
            session_id: sid.into(),
        }))
    }

    fn error(sid: &str, code: &str, message: &str) -> ServerMessage {
        msg(server_message::Payload::Error(ProtoError {
            code: code.into(),
            message: message.into(),
            session_id: sid.into(),
            ..Default::default()
        }))
    }

    fn usage() -> ServerMessage {
        msg(server_message::Payload::UsageInfo(UsageInfo {
            input_tokens: 1,
            output_tokens: 2,
            tool_calls: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost: 0.0,
            ..Default::default()
        }))
    }

    // ===== TextDelta / ThinkingBlock =====

    #[test]
    fn text_delta_parent_produces_agent_message_chunk() {
        let mut c = ctx();
        let out = translate(&text_delta(PARENT, "default", "你好"), &mut c);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::AgentMessageChunk(chunk) => {
                    match &chunk.content {
                        ContentBlock::Text(t) => assert_eq!(t.text, "你好"),
                        other => panic!("expected Text content, got {other:?}"),
                    }
                    assert!(chunk.message_id.is_some());
                }
                other => panic!("expected AgentMessageChunk, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn text_delta_child_suppressed() {
        let mut c = ctx();
        assert!(translate(&text_delta(CHILD, "explorer", "子输出"), &mut c).is_empty());
    }

    #[test]
    fn thinking_parent_chunk_carries_incremental_text_and_child_suppressed() {
        let mut c = ctx();
        let (mid, text) = thought_chunk_of(&translate(&thinking(PARENT, "思考中"), &mut c));
        assert_eq!(text, "思考中");
        assert!(!mid.0.is_empty(), "思考 chunk 应携带 messageId");
        assert!(
            translate(&thinking(CHILD, "子思考"), &mut c).is_empty(),
            "子 agent 思考帧应被抑制(与 TextDelta 策略对齐)"
        );
    }

    /// 回归护栏(问题一):`ThinkingBlock.thinking` 是**增量**语义,消费方对同一
    /// `messageId` 追加。连续增量帧拼接必须还原全文,且总字符数恰等于全文长度——
    /// 覆盖边界：本断言只证明 ACP 自身「messageId 不轮换 + 逐帧透传」；
    /// 它够不到上游 core 是否又改回快照发射，也够不到客户端（Zed）的渲染行为。
    #[test]
    fn thinking_incremental_frames_append_to_same_message_id_without_duplication() {
        // 10 段增量,每段仅携带相对上一帧的新增部分
        let parts: Vec<String> = (0..10).map(|i| format!("增量片段{i}；")).collect();
        let full: String = parts.concat();

        let mut c = ctx();
        let mut mids = Vec::new();
        let mut streamed = String::new();
        for part in &parts {
            let (mid, text) = thought_chunk_of(&translate(&thinking(PARENT, part), &mut c));
            mids.push(mid);
            streamed.push_str(&text);
        }

        assert!(
            mids.windows(2).all(|w| w[0] == w[1]),
            "同一思考流的所有 chunk 必须共享 messageId"
        );
        assert_eq!(streamed, full, "增量帧按序追加应还原全文,无重复");
        assert_eq!(
            streamed.chars().count(),
            full.chars().count(),
            "总字符数应等于全文长度,不得出现平方级膨胀"
        );
    }

    // ===== messageId 轮换(§7.3 近似策略) =====

    #[test]
    fn message_id_stable_within_agent_and_rotates_on_change_or_tool_frame() {
        let mut c = ctx();
        let mid_of = |out: &Vec<Outbound>| match &out[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::AgentMessageChunk(chunk) => chunk.message_id.clone().unwrap(),
                other => panic!("expected chunk, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        };

        let a1 = mid_of(&translate(&text_delta(PARENT, "default", "a"), &mut c));
        let a2 = mid_of(&translate(&text_delta(PARENT, "default", "b"), &mut c));
        assert_eq!(a1, a2, "同一 agent 连续文本 messageId 稳定");

        let b1 = mid_of(&translate(&text_delta(PARENT, "explorer", "c"), &mut c));
        assert_ne!(a1, b1, "agent 切换应轮换 messageId");

        translate(&tool_call(PARENT, "default", "call-1", "bash"), &mut c);
        let c1 = mid_of(&translate(&text_delta(PARENT, "default", "d"), &mut c));
        assert_ne!(b1, c1, "工具帧插入后应轮换 messageId");
    }

    // ===== ToolCall / ToolResult =====

    #[test]
    fn tool_call_kind_mapping() {
        let cases: &[(&str, ToolKind)] = &[
            ("read_file", ToolKind::Read),
            ("grep", ToolKind::Search),
            ("glob", ToolKind::Search),
            ("codegraph_search", ToolKind::Search),
            ("write_file", ToolKind::Edit),
            ("edit_file", ToolKind::Edit),
            ("bash", ToolKind::Execute),
            ("fetch_web", ToolKind::Fetch),
            ("task", ToolKind::Other),
            ("mystery_tool", ToolKind::Other),
        ];
        for (name, expected) in cases {
            let mut c = ctx();
            let out = translate(&tool_call(PARENT, "default", "call-x", name), &mut c);
            match &out[0] {
                Outbound::Update(u) => match u.as_ref() {
                    SessionUpdate::ToolCall(tc) => {
                        assert_eq!(tc.kind, *expected, "tool {name}");
                        assert_eq!(tc.status, ToolCallStatus::Pending);
                    }
                    other => panic!("expected ToolCall for {name}, got {other:?}"),
                },
                other => panic!("expected Update, got {other:?}"),
            }
        }
    }

    #[test]
    fn tool_call_child_still_streamed() {
        // 子 agent 工具帧刻意流式(§8.4:能看到子 agent 在做什么)
        let mut c = ctx();
        let out = translate(&tool_call(CHILD, "explorer", "call-c", "grep"), &mut c);
        assert!(matches!(
            &out[0],
            Outbound::Update(u) if matches!(u.as_ref(), SessionUpdate::ToolCall(_))
        ));
    }

    #[test]
    fn tool_result_completed_and_failed() {
        let mut c = ctx();
        let ok = translate(&tool_result(PARENT, "call-1", "输出内容", false), &mut c);
        match &ok[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::ToolCallUpdate(u) => {
                    assert_eq!(u.fields.status, Some(ToolCallStatus::Completed));
                }
                other => panic!("expected ToolCallUpdate, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }

        let bad = translate(&tool_result(PARENT, "call-2", "报错了", true), &mut c);
        match &bad[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::ToolCallUpdate(u) => {
                    assert_eq!(u.fields.status, Some(ToolCallStatus::Failed));
                }
                other => panic!("expected ToolCallUpdate, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_long_output_truncated_keeping_head_and_tail() {
        let mut c = ctx();
        let long = format!("{}middle{}", "h".repeat(5000), "t".repeat(5000));
        let out = translate(&tool_result(PARENT, "call-3", &long, false), &mut c);
        match &out[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::ToolCallUpdate(u) => {
                    let content = u.fields.content.as_ref().unwrap();
                    match &content[0] {
                        ToolCallContent::Content(c) => match &c.content {
                            ContentBlock::Text(t) => {
                                assert!(t.text.starts_with("hhhh"), "应保留首部");
                                assert!(t.text.ends_with("tttt"), "应保留尾部");
                                assert!(t.text.contains("已截断"), "应有截断提示");
                                assert!(!t.text.contains("middle"), "中段应被截去");
                            }
                            other => panic!("expected Text content, got {other:?}"),
                        },
                        other => panic!("expected Content variant, got {other:?}"),
                    }
                }
                other => panic!("expected ToolCallUpdate, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }
    }

    // ===== UserQuery 旁路(§8.4 特例) =====

    #[test]
    fn user_query_empty_options_routes_to_approval() {
        let mut c = ctx();
        let out = translate(&user_query(vec![]), &mut c);
        assert!(matches!(&out[0], Outbound::ApprovalNeeded(_)));
    }

    #[test]
    fn user_query_with_options_routes_to_question() {
        let mut c = ctx();
        let out = translate(&user_query(vec!["A".into(), "B".into()]), &mut c);
        assert!(matches!(&out[0], Outbound::QuestionNeeded(_)));
    }

    // ===== Done / Error 终止判定(session_id 主判据) =====

    #[test]
    fn done_parent_finishes_end_turn_and_child_suppressed() {
        let mut c = ctx();
        let out = translate(&done(PARENT), &mut c);
        assert!(matches!(&out[0], Outbound::FinishTurn(StopReason::EndTurn)));
        assert!(
            translate(&done(CHILD), &mut c).is_empty(),
            "子 Done 不终止父 turn"
        );
    }

    #[test]
    fn error_parent_cancelled_by_code_or_message() {
        let mut c = ctx();
        let by_code = translate(&error(PARENT, CANCELLED_CODE, "agent cancelled"), &mut c);
        assert!(matches!(
            &by_code[0],
            Outbound::FinishTurn(StopReason::Cancelled)
        ));

        // 双向匹配:message 含 "cancelled" 也判定取消(文案漂移防御)
        let by_message = translate(&error(PARENT, "InternalError", "agent cancelled"), &mut c);
        assert!(matches!(
            &by_message[0],
            Outbound::FinishTurn(StopReason::Cancelled)
        ));
    }

    #[test]
    fn error_parent_other_refuses_with_error_text() {
        let mut c = ctx();
        let out = translate(&error(PARENT, "ProviderError", "boom"), &mut c);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Outbound::ErrorText(m) if m == "boom"));
        assert!(matches!(&out[1], Outbound::FinishTurn(StopReason::Refusal)));
    }

    #[test]
    fn error_child_never_terminates_parent_turn() {
        let mut c = ctx();
        assert!(translate(&error(CHILD, "ProviderError", "子失败"), &mut c).is_empty());
    }

    // ===== M1 抑制 =====

    #[test]
    fn usage_and_unknown_variants_suppressed_in_m1() {
        let mut c = ctx();
        assert!(translate(&usage(), &mut c).is_empty());
        // 未知/新增 proto 变体安全抑制(通配分支)
        assert!(translate(&ServerMessage { payload: None }, &mut c).is_empty());
    }

    // ===== VerificationUpdate =====

    fn verification(sid: &str, stage: &str, is_final: bool, passed: bool) -> ServerMessage {
        msg(server_message::Payload::VerificationUpdate(
            visp_proto::visp::VerificationUpdate {
                stage: stage.into(),
                round: 1,
                max_rounds: 2,
                summary: "核心场景已实际验证".into(),
                evidence: vec![visp_proto::visp::VerificationEvidence {
                    check: "10 万行 CSV 查询".into(),
                    method: "构造大文件并执行".into(),
                    result: "通过".into(),
                }],
                is_final,
                passed,
                session_id: sid.into(),
                agent_name: "verifier".into(),
            },
        ))
    }

    fn chunk_text(out: &[Outbound]) -> String {
        match &out[0] {
            Outbound::Update(u) => match u.as_ref() {
                SessionUpdate::AgentMessageChunk(c) => match &c.content {
                    ContentBlock::Text(t) => t.text.clone(),
                    other => panic!("expected text, got {other:?}"),
                },
                other => panic!("expected AgentMessageChunk, got {other:?}"),
            },
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn verification_progress_and_final_pass_visible_to_client() {
        let mut c = ctx();
        let started = translate(&verification(PARENT, "started", false, false), &mut c);
        assert_eq!(started.len(), 1);
        assert!(chunk_text(&started).contains("独立验证开始"));
        assert_eq!(c.take_verification(), None);

        let passed = translate(&verification(PARENT, "passed", true, true), &mut c);
        let text = chunk_text(&passed);
        assert!(text.contains("验证通过"));
        assert!(text.contains("10 万行 CSV 查询"), "ACP 文本必须带证据");
        assert_eq!(c.take_verification(), Some(VerificationStatus::Passed));

        // Done 收尾仍在,最终状态可由 prompt_task 写入 _meta。
        let done_out = translate(&done(PARENT), &mut c);
        assert!(matches!(
            &done_out[0],
            Outbound::FinishTurn(StopReason::EndTurn)
        ));
    }

    #[test]
    fn verification_final_failure_is_distinguishable() {
        let mut c = ctx();
        let exhausted = translate(&verification(PARENT, "exhausted", true, false), &mut c);
        assert!(chunk_text(&exhausted).contains("验证未通过"));
        assert_eq!(c.take_verification(), Some(VerificationStatus::Failed));
    }

    #[test]
    fn verification_incomplete_is_not_reported_as_a_defect_verdict() {
        let mut c = ctx();
        let inconclusive = translate(&verification(PARENT, "inconclusive", true, false), &mut c);
        let text = chunk_text(&inconclusive);
        assert!(text.contains("验证未完成"));
        assert!(text.contains("不是发现缺陷"));
        assert_eq!(
            c.take_verification(),
            Some(VerificationStatus::Incomplete),
            "没做成必须与发现缺陷区分，但都不能当作通过"
        );
    }

    #[test]
    fn verification_from_child_session_is_not_suppressed() {
        let mut c = ctx();
        let out = translate(&verification(CHILD, "started", false, false), &mut c);
        assert_eq!(out.len(), 1, "验证进度即使来自子会话也必须可见");
    }

    // ===== StatusUpdate =====

    #[test]
    fn status_update_parent_chunk_and_child_suppressed() {
        let mut c = ctx();
        let su = msg(server_message::Payload::StatusUpdate(
            visp_proto::visp::StatusUpdate {
                message: "初始化中".into(),
                session_id: PARENT.into(),
                user_inputs: vec![],
                agent_name: String::new(),
                view_only: false,
            },
        ));
        let out = translate(&su, &mut c);
        assert!(matches!(
            &out[0],
            Outbound::Update(u) if matches!(u.as_ref(), SessionUpdate::AgentMessageChunk(_))
        ));

        let child_su = msg(server_message::Payload::StatusUpdate(
            visp_proto::visp::StatusUpdate {
                message: "子状态".into(),
                session_id: CHILD.into(),
                user_inputs: vec![],
                agent_name: "explorer".into(),
                view_only: false,
            },
        ));
        assert!(translate(&child_su, &mut c).is_empty());
    }
}
