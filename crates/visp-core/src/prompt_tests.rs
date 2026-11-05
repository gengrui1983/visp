use super::*;
use crate::context::ContextTrimmer;
use crate::message::Role;
use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

struct MockTrimmer {
    call_count: AtomicU32,
}

impl MockTrimmer {
    fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }
}

impl ContextTrimmer for MockTrimmer {
    fn trim(
        &self,
        history: &[Message],
        _max_ctx: u32,
        _overhead: u32,
        _output: u32,
    ) -> Vec<Message> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        history.to_vec()
    }
}

#[test]
fn test_system_message() {
    let trimmer = MockTrimmer::new();
    let messages = PromptBuilder::build(
        "You are helpful",
        "Be concise",
        &[],
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, Role::System);
    assert!(
        messages[0]
            .content
            .starts_with("You are helpful\n\n## Instructions\n\nBe concise")
    );
    assert!(messages[0].content.contains("[USER_QUERY]"));
    assert!(messages[0].content.contains("\"Other\""));
    assert!(messages[0].content.contains("Current Context"));
    assert!(messages[0].content.contains("/tmp"));
    assert!(messages[0].content.contains("2026-06-09"));
}

#[test]
fn test_history_order() {
    let trimmer = MockTrimmer::new();
    let history = vec![
        Message::user("Hello"),
        Message::assistant("Hi!"),
        Message::user("How are you?"),
    ];
    let messages = PromptBuilder::build(
        "You are helpful",
        "Be concise",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, Role::System);
    assert!(
        messages[0]
            .content
            .starts_with("You are helpful\n\n## Instructions\n\nBe concise")
    );
    assert_eq!(messages[1], Message::user("Hello"));
    assert_eq!(messages[2], Message::assistant("Hi!"));
    assert_eq!(messages[3], Message::user("How are you?"));
}

#[test]
fn test_empty_rules() {
    let trimmer = MockTrimmer::new();
    let messages = PromptBuilder::build(
        "You are helpful",
        "",
        &[],
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 1);
    assert!(messages[0].content.starts_with("You are helpful"));
    assert!(messages[0].content.contains("[USER_QUERY]"));
}

#[test]
fn test_empty_template() {
    let trimmer = MockTrimmer::new();
    let messages = PromptBuilder::build(
        "",
        "Be concise",
        &[],
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]
            .content
            .starts_with("## Instructions\n\nBe concise")
    );
    assert!(messages[0].content.contains("[USER_QUERY]"));
}

#[test]
fn test_empty_both() {
    let trimmer = MockTrimmer::new();
    let messages = PromptBuilder::build(
        "",
        "",
        &[],
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 1);
    assert!(messages[0].content.contains("[USER_QUERY]"));
}

#[test]
fn test_skip_context_messages_filtered() {
    let trimmer = MockTrimmer::new();
    let history = vec![
        Message::user("Hello"),
        Message {
            skip_context: true,
            ..Message::user("skip me")
        },
        Message::assistant("Hi!"),
    ];
    let messages = PromptBuilder::build(
        "system",
        "",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 3); // system + 2 non-skipped
    assert_eq!(messages[1], Message::user("Hello"));
    assert_eq!(messages[2], Message::assistant("Hi!"));
}

#[test]
fn test_user_query_instruction_present() {
    let instruction = user_query_instruction();
    assert!(instruction.contains("[USER_QUERY]"));
    assert!(instruction.contains("[/USER_QUERY]"));
    assert!(instruction.contains("\"Other\""));
    // 语气必须强硬，不能是 suggestion
    assert!(instruction.contains("MUST"));
    assert!(!instruction.contains("you can"));
}

#[test]
fn test_build_no_context_trimming() {
    let trimmer = MockTrimmer::new();
    let history = vec![
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
    ];
    let messages = PromptBuilder::build(
        "system",
        "",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    // None → no trimming: system + all 3 messages, trimmer not called
    assert_eq!(messages.len(), 4);
    assert_eq!(trimmer.call_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_build_with_context_trimming() {
    let trimmer = MockTrimmer::new();
    let history = vec![
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    let messages = PromptBuilder::build(
        "system",
        "",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        Some(3000),
        4096,
        &trimmer,
    );
    // trimmer is called
    assert!(!messages.is_empty());
    assert_eq!(messages[0].role, Role::System);
    assert!(messages.len() >= 2);
    assert!(trimmer.call_count.load(Ordering::SeqCst) > 0);
}

#[test]
fn test_build_passes_through_tool_output_when_no_trimming() {
    let trimmer = MockTrimmer::new();
    let long_output = "x".repeat(3000);
    let tool_msg = Message::tool(&long_output, "call_1");
    let history = vec![Message::user("u1"), tool_msg];
    let messages = PromptBuilder::build(
        "system",
        "",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    // No trimming means no truncation — tool output passes through as-is
    let tool_result = &messages[2]; // system + user + tool
    assert_eq!(tool_result.role, Role::Tool);
    assert_eq!(tool_result.content.len(), 3000);
    assert!(!tool_result.content.contains("truncated"));
    assert_eq!(trimmer.call_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_build_skip_context_excluded() {
    let trimmer = MockTrimmer::new();
    let skip_msg = Message {
        skip_context: true,
        ..Message::user("skip me")
    };
    let history = vec![Message::user("keep me"), skip_msg];
    let messages = PromptBuilder::build(
        "system",
        "",
        &history,
        Path::new("/tmp"),
        "2026-06-09",
        None,
        4096,
        &trimmer,
    );
    assert_eq!(messages.len(), 2); // system + 1 non-skipped
    assert_eq!(messages[1], Message::user("keep me"));
    assert_eq!(trimmer.call_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_default_prompt_contains_role() {
    assert!(DEFAULT_SYSTEM_PROMPT.contains("visp"));
}

#[test]
fn test_default_prompt_no_project_specific_content() {
    // Coding conventions 已移到 AGENTS.md，不应出现在 DEFAULT 中
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("Conventional Commits"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("简洁优先"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("TDD"));
}

#[test]
fn test_default_prompt_contains_interaction_rules() {
    assert!(DEFAULT_SYSTEM_PROMPT.contains("[USER_QUERY]"));
    // 引用详细说明，但不包含完整格式
    assert!(DEFAULT_SYSTEM_PROMPT.contains("see detailed instructions"));
    // 通用工具规则
    assert!(DEFAULT_SYSTEM_PROMPT.contains("tool results"));
}

#[test]
fn test_default_prompt_no_hardcoded_tools() {
    // 不应硬编码工具名，工具由动态指南渲染
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("ReadFile"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("Bash"));
}
