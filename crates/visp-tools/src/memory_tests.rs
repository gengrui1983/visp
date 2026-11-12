use super::*;
use std::path::PathBuf;
use std::sync::Arc;
use visp_core::memory::InMemoryMemoryStore;

fn ctx(session: Option<&str>) -> ToolContext {
    ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: session.map(str::to_string),
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    }
}

fn tool() -> MemoryTool {
    MemoryTool::new(Arc::new(InMemoryMemoryStore::new()))
}

#[tokio::test]
async fn record_then_search() {
    let tool = tool();
    let c = ctx(Some("s1"));

    let r = tool
        .execute(
            serde_json::json!({"action":"record","kind":"decision","content":"use Drop impl"}),
            &c,
        )
        .await;
    assert!(!r.is_error, "{}", r.content);

    let r = tool
        .execute(serde_json::json!({"action":"search","query":"DROP"}), &c)
        .await;
    assert!(!r.is_error);
    assert!(r.content.contains("decision"), "{}", r.content);
    assert!(r.content.contains("use Drop impl"), "{}", r.content);
}

#[tokio::test]
async fn list_returns_chronological_entries() {
    let tool = tool();
    let c = ctx(Some("s1"));
    for content in ["first", "second", "third"] {
        tool.execute(serde_json::json!({"action":"record","content":content}), &c)
            .await;
    }
    let r = tool
        .execute(serde_json::json!({"action":"list","limit":2}), &c)
        .await;
    assert!(!r.is_error);
    assert!(
        !r.content.contains("first"),
        "limit must drop oldest: {}",
        r.content
    );
    assert!(r.content.contains("second"));
    assert!(r.content.contains("third"));
}

#[tokio::test]
async fn missing_session_is_an_error() {
    let tool = tool();
    let r = tool
        .execute(serde_json::json!({"action":"list"}), &ctx(None))
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("session_id"));
}

#[tokio::test]
async fn record_requires_content() {
    let tool = tool();
    let r = tool
        .execute(serde_json::json!({"action":"record"}), &ctx(Some("s1")))
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("content"));
}

#[tokio::test]
async fn unknown_kind_falls_back_to_fact_and_duplicate_is_idempotent() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let tool = MemoryTool::new(store.clone());
    let c = ctx(Some("s1"));

    tool.execute(
        serde_json::json!({"action":"record","kind":"bogus","content":"same note"}),
        &c,
    )
    .await;
    let r = tool
        .execute(
            serde_json::json!({"action":"record","kind":"fact","content":"same note"}),
            &c,
        )
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert_eq!(store.count("s1").unwrap(), 1);
}

#[tokio::test]
async fn unknown_action_is_an_error() {
    let tool = tool();
    let r = tool
        .execute(serde_json::json!({"action":"delete"}), &ctx(Some("s1")))
        .await;
    assert!(r.is_error);
    assert!(r.content.contains("unknown memory action"));
}
