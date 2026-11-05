use super::*;
use crate::context::ContextTrimmer;
use crate::message::MessageType;
use crate::message::{Message, Role};
use crate::prompt::DEFAULT_SYSTEM_PROMPT;
use std::path::Path;

struct MockTrimmer;
impl ContextTrimmer for MockTrimmer {
    fn trim(&self, history: &[Message], _: u32, _: u32, _: u32) -> Vec<Message> {
        history.to_vec()
    }
}

// ── MockSessionStore for list_child_sessions tests ────────────────────

struct MockSessionStore {
    child_sessions: Vec<Session>,
}

impl SessionStore for MockSessionStore {
    fn create(&mut self, _session: Session) -> Result<(), SessionError> {
        Ok(())
    }
    fn get(&self, _session_id: &str) -> Result<Session, SessionError> {
        unimplemented!()
    }
    fn list(&self) -> Result<Vec<Session>, SessionError> {
        unimplemented!()
    }
    fn delete(&mut self, _session_id: &str) -> Result<(), SessionError> {
        Ok(())
    }
    fn update(&mut self, _session: Session) -> Result<(), SessionError> {
        Ok(())
    }
    fn get_messages(&self, _session_id: &str) -> Result<Vec<Message>, SessionError> {
        unimplemented!()
    }
    fn get_system_prompt(&self, _session_id: &str) -> Result<String, SessionError> {
        Ok("mock-template".into())
    }
    fn append_message(&mut self, _session_id: &str, _message: Message) -> Result<(), SessionError> {
        Ok(())
    }
    fn list_by_project(&self, _project_path: &str) -> Result<Vec<Session>, SessionError> {
        unimplemented!()
    }
    fn list_child_sessions(&self, _parent_id: &str) -> Result<Vec<Session>, SessionError> {
        Ok(self.child_sessions.clone())
    }
}

#[test]
fn session_store_list_child_sessions_signature() {
    let store = MockSessionStore {
        child_sessions: vec![],
    };
    let result = store.list_child_sessions("any-parent");
    assert_eq!(result.unwrap(), vec![]);
}

#[test]
fn mock_store_list_child_sessions_returns_empty() {
    let store = MockSessionStore {
        child_sessions: vec![],
    };
    let result = store.list_child_sessions("any");
    assert_eq!(result.unwrap(), vec![]);
}

#[test]
fn mock_store_list_child_sessions_returns_sessions() {
    let session = Session {
        id: "child-1".into(),
        project_path: PathBuf::from("/tmp"),
        status: SessionStatus::Idle,
        created_at: Instant::now(),
        created_at_unix: Some(1700000000000),
        history: vec![],
        last_user_message: None,
        config: LlmConfig::default(),
        system_prompt_template: "default".into(),
        approved_tools: HashSet::new(),
        agent_name: "sub-agent".into(),
        parent_id: Some("parent-1".into()),
        permission: vec![],
    };
    let store = MockSessionStore {
        child_sessions: vec![session.clone()],
    };
    let result = store.list_child_sessions("parent-1").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "child-1");
    assert_eq!(result[0].agent_name, "sub-agent");
    assert_eq!(result[0].parent_id, Some("parent-1".into()));
}

// ── Step 3: Session.created_at_unix ──

#[test]
fn test_session_created_at_unix_default_none() {
    let mut store = InMemorySessionStore::new();
    let session = Session {
        id: "test-u1".into(),
        project_path: PathBuf::from("/tmp"),
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
    let id = session.id.clone();
    store.create(session).unwrap();
    let got = store.get(&id).unwrap();
    assert_eq!(got.created_at_unix, None);
}

#[test]
fn test_session_created_at_unix_read_write() {
    let mut store = InMemorySessionStore::new();
    let session = Session {
        id: "test-u2".into(),
        project_path: PathBuf::from("/tmp"),
        status: SessionStatus::Idle,
        created_at: Instant::now(),
        created_at_unix: Some(1700000000000),
        history: vec![],
        last_user_message: None,
        config: LlmConfig::default(),
        system_prompt_template: "default".into(),
        approved_tools: HashSet::new(),
        agent_name: "default".into(),
        parent_id: None,
        permission: vec![],
    };
    let id = session.id.clone();
    store.create(session.clone()).unwrap();
    // get() currently returns reference to internal Session
    // We use get() via the trait to verify read
    let got = store.get(&id).unwrap();
    assert_eq!(got.created_at_unix, Some(1700000000000));

    // update
    let mut updated = session.clone();
    updated.created_at_unix = Some(1700000000001);
    store.update(updated).unwrap();
    let got = store.get(&id).unwrap();
    assert_eq!(got.created_at_unix, Some(1700000000001));
}

#[test]
fn test_session_created_at_unix_in_create() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    // Rust Instant used for runtime; created_at_unix is None unless explicitly set
    assert_eq!(session.created_at_unix, None);
    assert!(session.created_at.elapsed().as_secs() < 5);
}

// ── InMemorySessionStore CRUD ──────────────────────────────────────────

#[test]
fn test_in_memory_store_crud() {
    let mut store = InMemorySessionStore::new();
    let session = Session {
        id: "test-1".into(),
        project_path: PathBuf::from("/tmp"),
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

    // create
    store.create(session.clone()).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);

    // get
    let s = store.get("test-1").unwrap();
    assert_eq!(s.id, "test-1");

    // get not found
    assert!(store.get("nonexistent").is_err());

    // update
    let mut updated = session.clone();
    updated.status = SessionStatus::Running;
    store.update(updated.clone()).unwrap();
    let s = store.get("test-1").unwrap();
    assert_eq!(s.status, SessionStatus::Running);

    // duplicate create
    assert!(store.create(session.clone()).is_err());

    // delete
    store.delete("test-1").unwrap();
    assert!(store.get("test-1").is_err());
    assert_eq!(store.list().unwrap().len(), 0);

    // delete not found
    assert!(store.delete("nonexistent").is_err());
}

// ── SessionManager ────────────────────────────────────────────────────

#[test]
fn test_session_manager_create() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    // Isolate from real global skills
    let home = tempfile::TempDir::new().unwrap();
    // SAFETY: test-only, no other test uses set_var for HOME
    unsafe { std::env::set_var("HOME", home.path()) };
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.project_path, Path::new("/tmp"));
    assert!(
        session
            .system_prompt_template
            .starts_with(DEFAULT_SYSTEM_PROMPT)
    );
    // Skills are no longer injected into the system prompt; they are
    // surfaced via the SkillTool description instead.
    assert!(!session.system_prompt_template.contains("Available Skills"));
}

#[test]
fn test_session_manager_start_loop() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    assert_eq!(ctx.session_id, session.id);
    assert_eq!(ctx.working_dir, Path::new("/tmp"));

    let s = manager.get(&session.id).unwrap();
    assert_eq!(s.status, SessionStatus::Running);
}

#[test]
fn test_session_manager_start_loop_busy() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();

    match manager.start_loop(&session.id, &trimmer, None, None) {
        Err(SessionError::SessionBusy { session_id }) => {
            assert_eq!(session_id, session.id);
        }
        _ => panic!("expected SessionBusy"),
    }
}

#[test]
fn test_session_manager_finish_loop() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    manager
        .finish_loop(&session.id, SessionStatus::Completed)
        .unwrap();

    let s = manager.get(&session.id).unwrap();
    assert_eq!(s.status, SessionStatus::Completed);
}

#[test]
fn test_session_manager_delete_cancels() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    let token = ctx.cancel_token.clone();
    assert!(!token.is_cancelled());

    manager.delete(&session.id).unwrap();
    assert!(token.is_cancelled());
    assert!(manager.get(&session.id).is_err());
}

#[test]
fn test_session_manager_append_message() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let msg = Message {
        role: Role::User,
        kind: MessageType::User,
        content: "hello".into(),
        tool_call_id: None,
        tool_calls: None,
        tool_call_count: None,
        extra_blocks: None,
        skip_context: false,
        estimated_tokens: 0,
        actual_tokens_input: None,
        actual_tokens_output: None,
        actual_cache_read: None,
        actual_cache_write: None,
        actual_cost: None,
        provider_metadata: None,
        tool_result_is_error: None,
        tool_result_duration_ms: None,
        created_at: None,
        images: Vec::new(),
    };
    manager.append_message(&session.id, msg.clone()).unwrap();

    let s = manager.get(&session.id).unwrap();
    assert_eq!(s.history.len(), 1);
    assert_eq!(s.history[0].role, Role::User);
    assert_eq!(s.history[0].content, "hello");
}

#[test]
fn test_session_manager_update_config() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let new_config = LlmConfig {
        model: "gpt-4".into(),
        ..Default::default()
    };
    manager
        .update_config(&session.id, new_config.clone())
        .unwrap();

    let s = manager.get(&session.id).unwrap();
    assert_eq!(s.config.model, "gpt-4");
}

#[test]
fn test_session_approved_tools_empty_on_create() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    assert!(session.approved_tools.is_empty());
}

#[test]
fn test_session_approved_tools_clone() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session1 = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session1.id.clone();
    manager.add_approved_tool(&sid, "bash").unwrap();
    let s = manager.get(&sid).unwrap();
    assert!(s.approved_tools.contains("bash"));
}

#[test]
fn test_session_is_tool_approved() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sid = session.id.clone();

    // Before adding, not approved
    assert!(!manager.is_tool_approved(&sid, "bash"));

    // Add and verify
    manager.add_approved_tool(&sid, "bash").unwrap();
    assert!(manager.is_tool_approved(&sid, "bash"));

    // Other tool still not approved
    assert!(!manager.is_tool_approved(&sid, "write_file"));
}

// ── SessionManager::has_inflight_loop ─────────────────────────────────

#[test]
fn test_has_inflight_loop_true_after_start() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    assert!(!manager.has_inflight_loop(&session.id));

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    assert!(manager.has_inflight_loop(&session.id));
}

#[test]
fn test_has_inflight_loop_false_after_finish() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    manager
        .finish_loop(&session.id, SessionStatus::Completed)
        .unwrap();
    assert!(!manager.has_inflight_loop(&session.id));
}

#[test]
fn test_has_inflight_loop_false_after_cancel() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    assert!(manager.has_inflight_loop(&session.id));

    manager.cancel_agent(&session.id);
    assert!(!manager.has_inflight_loop(&session.id));
}

#[test]
fn test_has_inflight_loop_unknown_session() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    assert!(!manager.has_inflight_loop("nonexistent-session"));
}

#[test]
fn test_has_inflight_loop_idle_session() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    assert!(!manager.has_inflight_loop(&session.id));
}

#[test]
fn test_finish_loop_removes_token_without_cancelling() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let session = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let ctx = manager
        .start_loop(&session.id, &trimmer, None, None)
        .unwrap();
    let token = ctx.cancel_token.clone();

    manager
        .finish_loop(&session.id, SessionStatus::Completed)
        .unwrap();
    assert!(!manager.has_inflight_loop(&session.id));
    assert!(!token.is_cancelled());
}

#[test]
fn test_has_inflight_loop_counts_sub_session() {
    let manager = SessionManager::new(InMemorySessionStore::new());
    let parent = manager
        .create(Path::new("/tmp"), LlmConfig::default())
        .unwrap();
    let sub = manager
        .create_sub(SubSessionParams {
            parent_id: Some(parent.id.clone()),
            agent_name: "fixer".into(),
            permission: vec![],
            session_id: None,
            project_path: PathBuf::from("/tmp"),
            config: LlmConfig::default(),
            approved_tools: HashSet::new(),
        })
        .unwrap();

    let trimmer: Arc<dyn ContextTrimmer + Send + Sync> = Arc::new(MockTrimmer);
    let _ctx = manager.start_loop(&sub.id, &trimmer, None, None).unwrap();
    assert!(manager.has_inflight_loop(&sub.id));
}
