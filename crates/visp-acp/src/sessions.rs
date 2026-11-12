//! 会话注册表与归属路由(Step 6)。
//!
//! - 注册表:ACP `sessionId` ↔ visp `session_id`,维护 M1「同进程单在途父
//!   prompt」约束(§8.4:proto 无 parent 关联,并发多父会导致子事件无法归属)。
//! - 归属路由:入站 `ServerMessage` 的处理分派——`UserQuery` 一律旁路进桥接
//!   (§8.4 特例),无在途的迟到事件丢弃,其余交给事件泵翻译。

use std::collections::HashMap;

use visp_proto::visp::{ServerMessage, server_message};

/// ACP 会话条目。
#[derive(Debug, Clone)]
pub struct SessionEntry {
    /// 对应的 visp session id(本期直接复用为 ACP sessionId,§6.3)
    pub visp_session_id: String,
}

/// 会话注册表:M1 同进程单在途父 prompt。
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<String, SessionEntry>,
    /// 当前在途父 prompt 的 ACP sessionId(全局唯一,M1 约束)
    inflight: Option<String>,
}

/// 并发 prompt 拒绝原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptBusy {
    /// 同一会话已有在途 turn
    SessionBusy,
    /// 其他会话有在途 turn(M1 单在途父 prompt 约束)
    AnotherInFlight,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个 ACP 会话(`session/new` 成功后调用)。
    pub fn register(
        &mut self,
        acp_session_id: impl Into<String>,
        visp_session_id: impl Into<String>,
    ) {
        self.sessions.insert(
            acp_session_id.into(),
            SessionEntry {
                visp_session_id: visp_session_id.into(),
            },
        );
    }

    /// 由 ACP sessionId 查 visp session id。
    pub fn visp_id(&self, acp_session_id: &str) -> Option<&str> {
        self.sessions
            .get(acp_session_id)
            .map(|e| e.visp_session_id.as_str())
    }

    /// 开始一个父 prompt。同会话已有在途、或任一其他会话在途 → 拒绝。
    pub fn try_begin_prompt(&mut self, acp_session_id: &str) -> Result<(), PromptBusy> {
        if !self.sessions.contains_key(acp_session_id) {
            // 未知会话同样拒绝(prompt 只对已注册会话有效)
            return Err(PromptBusy::SessionBusy);
        }
        match &self.inflight {
            Some(inflight) if inflight == acp_session_id => Err(PromptBusy::SessionBusy),
            Some(_) => Err(PromptBusy::AnotherInFlight),
            None => {
                self.inflight = Some(acp_session_id.to_string());
                Ok(())
            }
        }
    }

    /// 结束在途 prompt(收到收尾信号或兜底超时后调用)。
    pub fn end_prompt(&mut self, acp_session_id: &str) {
        if self.inflight.as_deref() == Some(acp_session_id) {
            self.inflight = None;
        }
    }

    /// 当前在途父 prompt 的 ACP sessionId(无则 None)。
    pub fn inflight(&self) -> Option<&str> {
        self.inflight.as_deref()
    }

    /// 清理会话条目(M3 `session/delete`;M1 预留)。
    pub fn remove(&mut self, acp_session_id: &str) -> bool {
        if self.inflight.as_deref() == Some(acp_session_id) {
            self.inflight = None;
        }
        self.sessions.remove(acp_session_id).is_some()
    }
}

// ===== 6b:归属路由 =====

/// 入站事件的处理分派。
#[derive(Debug)]
pub enum Route {
    /// 交给事件泵翻译(翻译器按 session_id 判定父/子,归属主判据只有 session_id,V9)
    Pump(ServerMessage),
    /// 绕过归属,直接进审批/提问桥接(§8.4 UserQuery 特例)
    Bridge(ServerMessage),
    /// 丢弃(迟到事件/未知 session),记 stderr 警告
    Drop,
}

/// 提取事件的 session_id(所有带 session_id 的变体;缺失返回 None)。
pub fn extract_session_id(msg: &ServerMessage) -> Option<&str> {
    let payload = msg.payload.as_ref()?;
    match payload {
        server_message::Payload::TextDelta(m) => Some(&m.session_id),
        server_message::Payload::ToolCall(m) => Some(&m.session_id),
        server_message::Payload::ToolResult(m) => Some(&m.session_id),
        server_message::Payload::StatusUpdate(m) => Some(&m.session_id),
        server_message::Payload::Error(m) => Some(&m.session_id),
        server_message::Payload::Done(m) => Some(&m.session_id),
        server_message::Payload::UserQuery(m) => Some(&m.session_id),
        server_message::Payload::ThinkingBlock(m) => Some(&m.session_id),
        server_message::Payload::UserMessage(m) => Some(&m.session_id),
        server_message::Payload::UsageInfo(m) => Some(&m.session_id),
        server_message::Payload::UsageDelta(m) => Some(&m.session_id),
        server_message::Payload::ImageBlock(m) => Some(&m.session_id),
        server_message::Payload::ImageError(m) => Some(&m.session_id),
        server_message::Payload::VerificationUpdate(m) => Some(&m.session_id),
    }
}

/// 归属路由一条入站事件。
///
/// 规则(§8.4,三轮修正):
/// 1. `UserQuery` → `Bridge`(一律绕过归属,子 agent 审批/提问也必须可见);
/// 2. 无在途父 prompt(迟到事件:子事件晚于父 Done、兜底收尾后的迟到父信号)→ `Drop`;
/// 3. 其余 → `Pump`(翻译器按 `session_id == 在途父` 判定父子)。
pub fn route(msg: ServerMessage, registry: &SessionRegistry) -> Route {
    // UserQuery 特例优先于一切归属判定
    if matches!(
        msg.payload.as_ref(),
        Some(server_message::Payload::UserQuery(_))
    ) {
        return Route::Bridge(msg);
    }
    // 无在途父 prompt → 迟到/未知事件,丢弃并警告
    let Some(inflight) = registry.inflight() else {
        tracing::warn!(
            session = ?extract_session_id(&msg),
            "dropping late event: no in-flight parent prompt"
        );
        return Route::Drop;
    };
    // 有在途:交给事件泵(父/子判定由翻译器按 session_id 完成)
    let _ = inflight;
    Route::Pump(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use visp_proto::visp::{Done, UserQuery, server_message};

    fn done(session_id: &str) -> ServerMessage {
        ServerMessage {
            payload: Some(server_message::Payload::Done(Done {
                session_id: session_id.into(),
            })),
        }
    }

    fn user_query(session_id: &str, options: Vec<String>) -> ServerMessage {
        ServerMessage {
            payload: Some(server_message::Payload::UserQuery(UserQuery {
                query_id: "q-1".into(),
                message: "选择?".into(),
                session_id: session_id.into(),
                options,
                allow_other: false,
            })),
        }
    }

    // ===== 6a:注册表与在途状态机 =====

    #[test]
    fn register_and_lookup() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        assert_eq!(r.visp_id("acp-1"), Some("visp-1"));
        assert_eq!(r.visp_id("unknown"), None);
    }

    #[test]
    fn same_session_concurrent_prompt_rejected() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        r.try_begin_prompt("acp-1").unwrap();
        assert_eq!(
            r.try_begin_prompt("acp-1"),
            Err(PromptBusy::SessionBusy),
            "同会话并发 prompt 应被拒"
        );
    }

    #[test]
    fn different_session_concurrent_prompt_rejected_m1() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        r.register("acp-2", "visp-2");
        r.try_begin_prompt("acp-1").unwrap();
        assert_eq!(
            r.try_begin_prompt("acp-2"),
            Err(PromptBusy::AnotherInFlight),
            "M1:不同会话并发 prompt 也应被拒"
        );
    }

    #[test]
    fn end_prompt_resets_state() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        r.try_begin_prompt("acp-1").unwrap();
        r.end_prompt("acp-1");
        assert_eq!(r.inflight(), None);
        r.try_begin_prompt("acp-1").unwrap(); // 复位后可再次开始
    }

    #[test]
    fn remove_clears_entry_and_inflight() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        r.try_begin_prompt("acp-1").unwrap();
        assert!(r.remove("acp-1"));
        assert_eq!(r.visp_id("acp-1"), None);
        assert_eq!(r.inflight(), None, "删除在途会话应同时清空在途状态");
    }

    // ===== 6b:归属路由 =====

    #[test]
    fn user_query_bypasses_routing_in_any_state() {
        let mut r = SessionRegistry::new();
        // 无在途:UserQuery 仍旁路(子 agent 审批/提问必须可见,§8.4 特例)
        assert!(matches!(
            route(user_query("visp-child", vec![]), &r),
            Route::Bridge(_)
        ));
        r.register("acp-1", "visp-1");
        r.try_begin_prompt("acp-1").unwrap();
        // 在途:同样旁路
        assert!(matches!(
            route(user_query("visp-child", vec![]), &r),
            Route::Bridge(_)
        ));
    }

    #[test]
    fn late_events_without_inflight_are_dropped() {
        let r = SessionRegistry::new();
        // 兜底收尾后迟到的父 Done
        assert!(matches!(route(done("visp-1"), &r), Route::Drop));
        // 迟到的子事件
        assert!(matches!(route(done("visp-child"), &r), Route::Drop));
    }

    #[test]
    fn events_pumped_while_inflight() {
        let mut r = SessionRegistry::new();
        r.register("acp-1", "visp-1");
        r.try_begin_prompt("acp-1").unwrap();
        // 父事件与子事件都交给事件泵(父子判定由翻译器按 session_id 完成)
        assert!(matches!(route(done("visp-1"), &r), Route::Pump(_)));
        assert!(matches!(route(done("visp-child"), &r), Route::Pump(_)));
    }

    #[test]
    fn extract_session_id_covers_done_and_user_query() {
        assert_eq!(extract_session_id(&done("s-1")), Some("s-1"));
        assert_eq!(extract_session_id(&user_query("s-2", vec![])), Some("s-2"));
    }
}
