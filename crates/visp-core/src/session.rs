use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::AgentLoopContext;
use crate::agent::Envelope;
use crate::agent_definition::PermissionRule;
use crate::error::SessionError;
use crate::message::Message;
use crate::message::Role;
use crate::provider::LlmConfig;

pub use visp_config::path::home_dir;
pub use visp_config::prompt::load_system_prompt_template;
pub use visp_config::skills::load_skills;
pub use visp_config::skills::strip_frontmatter;

/// 会话状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Running,
    Completed,
    Error,
}

/// 会话
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub project_path: PathBuf,
    pub status: SessionStatus,
    pub created_at: Instant,
    /// Unix 毫秒时间戳，用于 DB 持久化（Option 以兼容运行时无法确定时间的场景）
    pub created_at_unix: Option<i64>,
    pub history: Vec<Message>,
    /// 最后一条用户消息内容（用于 /list 显示），截断到 80 字符
    pub last_user_message: Option<String>,
    pub config: LlmConfig,
    pub system_prompt_template: String,
    /// 已审批的工具名称集合（Always Allow）
    pub approved_tools: HashSet<String>,
    /// 当前使用的 Agent 名称
    pub agent_name: String,
    /// 父 Session ID（子 Session 用）
    pub parent_id: Option<String>,
    /// 运行时权限规则集
    pub permission: Vec<PermissionRule>,
}

/// 会话存储抽象 trait
pub trait SessionStore: Send {
    fn create(&mut self, session: Session) -> Result<(), SessionError>;
    fn get(&self, session_id: &str) -> Result<Session, SessionError>;
    fn list(&self) -> Result<Vec<Session>, SessionError>;
    fn delete(&mut self, session_id: &str) -> Result<(), SessionError>;
    fn update(&mut self, session: Session) -> Result<(), SessionError>;

    /// 获取会话的全部消息
    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError>;
    /// 轻量查询：仅获取会话的 system_prompt_template，避免全量加载消息
    fn get_system_prompt(&self, session_id: &str) -> Result<String, SessionError>;
    /// 追加一条消息到会话
    fn append_message(&mut self, session_id: &str, message: Message) -> Result<(), SessionError>;
    /// 按项目路径过滤会话列表
    fn list_by_project(&self, project_path: &str) -> Result<Vec<Session>, SessionError>;
    /// 获取指定父会话的所有子会话
    fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError>;
}

/// 内存会话存储
pub struct InMemorySessionStore {
    sessions: HashMap<String, Session>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn create(&mut self, session: Session) -> Result<(), SessionError> {
        if self.sessions.contains_key(&session.id) {
            return Err(SessionError::AlreadyExists(session.id));
        }
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    fn get(&self, session_id: &str) -> Result<Session, SessionError> {
        self.sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    fn list(&self) -> Result<Vec<Session>, SessionError> {
        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        for session in &mut sessions {
            session.last_user_message = session
                .history
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| {
                    if m.content.chars().count() > 80 {
                        format!("{}...", m.content.chars().take(80).collect::<String>())
                    } else {
                        m.content.clone()
                    }
                });
        }
        Ok(sessions)
    }

    fn delete(&mut self, session_id: &str) -> Result<(), SessionError> {
        self.sessions
            .remove(session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        Ok(())
    }

    fn update(&mut self, session: Session) -> Result<(), SessionError> {
        if !self.sessions.contains_key(&session.id) {
            return Err(SessionError::NotFound(session.id));
        }
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        self.sessions
            .get(session_id)
            .map(|s| s.history.clone())
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    fn get_system_prompt(&self, session_id: &str) -> Result<String, SessionError> {
        self.sessions
            .get(session_id)
            .map(|s| s.system_prompt_template.clone())
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    fn append_message(&mut self, session_id: &str, message: Message) -> Result<(), SessionError> {
        self.sessions
            .get_mut(session_id)
            .map(|s| s.history.push(message))
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    fn list_by_project(&self, project_path: &str) -> Result<Vec<Session>, SessionError> {
        let target = PathBuf::from(project_path);
        let mut sessions: Vec<Session> = self
            .sessions
            .values()
            .filter(|s| s.project_path == target)
            .cloned()
            .collect();
        for session in &mut sessions {
            session.last_user_message = session
                .history
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| {
                    if m.content.chars().count() > 80 {
                        format!("{}...", m.content.chars().take(80).collect::<String>())
                    } else {
                        m.content.clone()
                    }
                });
        }
        Ok(sessions)
    }

    fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError> {
        Ok(self
            .sessions
            .values()
            .filter(|s| s.parent_id.as_deref() == Some(parent_id))
            .cloned()
            .collect())
    }
}

impl SessionStore for Box<dyn SessionStore> {
    fn create(&mut self, session: Session) -> Result<(), SessionError> {
        (**self).create(session)
    }
    fn get(&self, session_id: &str) -> Result<Session, SessionError> {
        (**self).get(session_id)
    }
    fn list(&self) -> Result<Vec<Session>, SessionError> {
        (**self).list()
    }
    fn delete(&mut self, session_id: &str) -> Result<(), SessionError> {
        (**self).delete(session_id)
    }
    fn update(&mut self, session: Session) -> Result<(), SessionError> {
        (**self).update(session)
    }
    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        (**self).get_messages(session_id)
    }
    fn get_system_prompt(&self, session_id: &str) -> Result<String, SessionError> {
        (**self).get_system_prompt(session_id)
    }
    fn append_message(&mut self, session_id: &str, message: Message) -> Result<(), SessionError> {
        (**self).append_message(session_id, message)
    }
    fn list_by_project(&self, project_path: &str) -> Result<Vec<Session>, SessionError> {
        (**self).list_by_project(project_path)
    }
    fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError> {
        (**self).list_child_sessions(parent_id)
    }
}

/// 参数：创建子会话
pub struct SubSessionParams {
    pub parent_id: Option<String>,
    pub agent_name: String,
    pub permission: Vec<PermissionRule>,
    /// Some = 复用已有 session，None = 新 UUID
    pub session_id: Option<String>,
    pub project_path: PathBuf,
    pub config: LlmConfig,
    /// 从父会话继承的已审批工具集合（Always Allow），使子 agent 无需重复审批
    pub approved_tools: HashSet<String>,
}

/// 会话管理器
pub struct SessionManager {
    store: Arc<Mutex<dyn SessionStore>>,
    running_tokens: Mutex<HashMap<String, CancellationToken>>,
}

impl SessionManager {
    pub fn new(store: impl SessionStore + 'static) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            running_tokens: Mutex::new(HashMap::new()),
        }
    }

    /// 创建会话，自动加载系统 prompt 模板
    pub fn create(&self, project_path: &Path, config: LlmConfig) -> Result<Session, SessionError> {
        let id = Uuid::new_v4().to_string();
        let prompt_template = load_system_prompt_template(project_path);

        let session = Session {
            id,
            project_path: project_path.to_path_buf(),
            status: SessionStatus::Idle,
            created_at: Instant::now(),
            created_at_unix: None,
            history: Vec::new(),
            last_user_message: None,
            config,
            system_prompt_template: prompt_template,
            approved_tools: HashSet::new(),
            agent_name: "default".to_string(),
            parent_id: None,
            permission: Vec::new(),
        };

        let mut store = self.store.lock().unwrap();
        let cloned = session.clone();
        store.create(cloned)?;

        Ok(session)
    }

    /// 创建子会话（多 Agent）
    /// session_id 为 Some 时复用已有 session，否则生成新 UUID
    pub fn create_sub(&self, params: SubSessionParams) -> Result<Session, SessionError> {
        // 复用
        if let Some(sid) = &params.session_id {
            let store = self.store.lock().unwrap();
            if let Ok(existing) = store.get(sid) {
                return Ok(existing);
            }
        }

        let id = params
            .session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let base = load_system_prompt_template(&params.project_path);
        let system_prompt_template = format!(
            "You are agent \"{}\" working on a project.\n\n{base}",
            params.agent_name
        );

        let session = Session {
            id,
            project_path: params.project_path,
            status: SessionStatus::Idle,
            created_at: Instant::now(),
            created_at_unix: None,
            history: Vec::new(),
            last_user_message: None,
            config: params.config,
            system_prompt_template,
            approved_tools: params.approved_tools,
            agent_name: params.agent_name,
            parent_id: params.parent_id,
            permission: params.permission,
        };

        let mut store = self.store.lock().unwrap();
        let cloned = session.clone();
        store.create(cloned)?;
        Ok(session)
    }

    /// 删除会话，如有运行中的 agent 则先 cancel
    pub fn delete(&self, id: &str) -> Result<(), SessionError> {
        // Cancel running token if any
        if let Some(token) = self.running_tokens.lock().unwrap().remove(id) {
            token.cancel();
        }

        let mut store = self.store.lock().unwrap();
        store.delete(id)
    }

    /// 启动 agent 循环，检查状态必须为 Idle
    /// 启动 agent 循环。
    ///
    /// `global_tx` / `permission_rules` 用于多 Agent 模式；
    /// 单 Agent 模式（测试 / 旧路径）传 `None` 即可。
    pub fn start_loop(
        &self,
        id: &str,
        context_trimmer: &Arc<dyn crate::context::ContextTrimmer + Send + Sync>,
        global_tx: Option<mpsc::Sender<Envelope>>,
        permission_rules: Option<Arc<Vec<PermissionRule>>>,
    ) -> Result<AgentLoopContext, SessionError> {
        let token = CancellationToken::new();

        let mut store = self.store.lock().unwrap();
        let session = store.get(id)?;

        if session.status != SessionStatus::Idle {
            return Err(SessionError::SessionBusy {
                session_id: id.to_string(),
            });
        }

        let mut updated = session.clone();
        updated.status = SessionStatus::Running;
        store.update(updated)?;
        drop(store);

        self.running_tokens
            .lock()
            .unwrap()
            .insert(id.to_string(), token.clone());

        Ok(AgentLoopContext {
            session_id: id.to_string(),
            history: session.history,
            working_dir: session.project_path,
            config: session.config,
            cancel_token: token,
            context_trimmer: Arc::clone(context_trimmer),
            global_tx,
            permission_rules,
            agent_kind: crate::agent::AgentKind::Primary,
            depth: 0,
            parent_session_id: session.parent_id.clone().unwrap_or_else(|| id.to_string()),
        })
    }

    /// 结束 agent 循环，切换会话状态，清理 token
    pub fn finish_loop(&self, id: &str, status: SessionStatus) -> Result<(), SessionError> {
        let mut store = self.store.lock().unwrap();
        let mut session = store.get(id)?;
        session.status = status;
        store.update(session)?;
        drop(store);

        self.running_tokens.lock().unwrap().remove(id);
        Ok(())
    }

    /// 追加消息到会话历史
    pub fn append_message(&self, id: &str, msg: Message) -> Result<(), SessionError> {
        let mut store = self.store.lock().unwrap();
        store.append_message(id, msg)
    }

    /// 更新会话的 LLM 配置
    pub fn update_config(&self, id: &str, config: LlmConfig) -> Result<(), SessionError> {
        let mut store = self.store.lock().unwrap();
        let mut session = store.get(id)?;
        session.config = config;
        store.update(session)
    }

    /// 追加内容到会话的 system_prompt_template
    pub fn append_system_prompt_template(&self, id: &str, extra: &str) -> Result<(), SessionError> {
        if extra.is_empty() {
            return Ok(());
        }
        let mut store = self.store.lock().unwrap();
        let mut session = store.get(id)?;
        session.system_prompt_template.push_str("\n\n");
        session.system_prompt_template.push_str(extra);
        store.update(session)
    }

    /// 列出所有会话
    pub fn list(&self) -> Result<Vec<Session>, SessionError> {
        let store = self.store.lock().unwrap();
        store.list()
    }

    /// 获取单个会话
    pub fn get(&self, id: &str) -> Result<Session, SessionError> {
        let store = self.store.lock().unwrap();
        store.get(id)
    }

    /// 获取会话的全部消息
    pub fn get_messages(&self, id: &str) -> Result<Vec<Message>, SessionError> {
        let store = self.store.lock().unwrap();
        store.get_messages(id)
    }

    /// 轻量查询：仅获取会话的 system_prompt_template（避免全量加载消息）
    pub fn get_system_prompt(&self, id: &str) -> Result<String, SessionError> {
        let store = self.store.lock().unwrap();
        store.get_system_prompt(id)
    }

    /// 获取指定父 session 的所有子 session（直接代理 SessionStore.list_child_sessions）
    pub fn list_child_sessions(&self, parent_id: &str) -> Result<Vec<Session>, SessionError> {
        let store = self.store.lock().unwrap();
        store.list_child_sessions(parent_id)
    }

    /// 取消运行中的 agent（取消 CancellationToken）
    /// 如果 session 未在运行中，则为 no-op
    pub fn cancel_agent(&self, id: &str) {
        if let Some(token) = self.running_tokens.lock().unwrap().remove(id) {
            token.cancel();
        }
    }

    /// 只读查询：按会话 id 判断是否存在在途循环的取消令牌
    pub fn has_inflight_loop(&self, session_id: &str) -> bool {
        self.running_tokens.lock().unwrap().contains_key(session_id)
    }

    /// 检查工具是否已被审批（Always Allow）
    pub fn is_tool_approved(&self, session_id: &str, tool_name: &str) -> bool {
        let store = self.store.lock().unwrap();
        match store.get(session_id) {
            Ok(session) => session.approved_tools.contains(tool_name),
            Err(_) => false,
        }
    }

    /// 将工具加入已审批集合（Always Allow）
    pub fn add_approved_tool(&self, session_id: &str, tool_name: &str) -> Result<(), SessionError> {
        let mut store = self.store.lock().unwrap();
        let mut session = store.get(session_id)?;
        session.approved_tools.insert(tool_name.to_string());
        store.update(session)
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
