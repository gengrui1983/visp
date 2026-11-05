use super::*;

// ── Step 1: MessageType ──

#[test]
fn test_message_type_serde() {
    let cases = vec![
        (MessageType::Text, "\"text\""),
        (MessageType::Thinking, "\"thinking\""),
        (MessageType::ToolCall, "\"tool_call\""),
        (MessageType::ToolResult, "\"tool_result\""),
        (MessageType::Error, "\"error\""),
        (MessageType::Status, "\"status\""),
        (MessageType::System, "\"system\""),
        (MessageType::User, "\"user\""),
    ];
    for (variant, expected) in cases {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, expected);
        let deserialized: MessageType = serde_json::from_str(expected).unwrap();
        assert_eq!(deserialized, variant);
    }
}

#[test]
fn test_user_message_kind() {
    let msg = Message::user("hi");
    assert_eq!(msg.kind, MessageType::User);
}

#[test]
fn test_system_message_kind() {
    let msg = Message::system("prompt");
    assert_eq!(msg.kind, MessageType::System);
}

#[test]
fn test_tool_message_kind() {
    let msg = Message::tool("out", "id");
    assert_eq!(msg.kind, MessageType::ToolResult);
}

// ── Wave 0 Task 0A: tool_result_duration_ms 构造接口 ──

#[test]
fn test_message_tool_accepts_duration_ms() {
    // 新构造接口：tool_with_duration 接受 duration_ms 参数并填入字段
    let msg = Message::tool_with_duration("out", "call_42", Some(123));
    assert_eq!(msg.kind, MessageType::ToolResult);
    assert_eq!(msg.role, Role::Tool);
    assert_eq!(msg.tool_call_id.as_deref(), Some("call_42"));
    assert_eq!(msg.tool_result_duration_ms, Some(123));
}

#[test]
fn test_message_tool_duration_none_when_not_supplied() {
    // 旧路径 Message::tool 不传 duration，字段保持 None（向后兼容）
    let msg = Message::tool("out", "call_1");
    assert_eq!(msg.tool_result_duration_ms, None);
    // 新接口传 None 也应保持 None
    let msg2 = Message::tool_with_duration("out", "call_2", None);
    assert_eq!(msg2.tool_result_duration_ms, None);
}

#[test]
fn test_assistant_message_kind() {
    let msg = Message::assistant("text");
    assert_eq!(msg.kind, MessageType::Text);
}

#[test]
fn test_tool_call_constructor() {
    let calls = vec![ToolCallRequest {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"test.txt"}"#.into(),
    }];
    let msg = Message::tool_call(calls.clone());
    assert_eq!(msg.kind, MessageType::ToolCall);
    assert_eq!(msg.role, Role::Assistant);
    assert!(msg.tool_calls.is_some());
    assert_eq!(msg.tool_calls.unwrap(), calls);
}

#[test]
fn test_thinking_constructor() {
    let msg = Message::thinking("thinking content");
    assert_eq!(msg.kind, MessageType::Thinking);
    assert_eq!(msg.content, "thinking content");
}

#[test]
fn test_error_constructor() {
    let msg = Message::error("something went wrong");
    assert_eq!(msg.kind, MessageType::Error);
    assert_eq!(msg.content, "something went wrong");
    assert_eq!(msg.role, Role::System);
}

#[test]
fn test_status_constructor() {
    let msg = Message::status("processing...");
    assert_eq!(msg.kind, MessageType::Status);
    assert_eq!(msg.content, "processing...");
    assert_eq!(msg.role, Role::System);
}

// ── Step 2: actual_* fields ──

#[test]
fn test_actual_fields_default_none() {
    let msg = Message::user("hi");
    assert_eq!(msg.actual_tokens_input, None);
    assert_eq!(msg.actual_tokens_output, None);
    assert_eq!(msg.actual_cache_read, None);
    assert_eq!(msg.actual_cache_write, None);
    assert_eq!(msg.actual_cost, None);
    assert_eq!(msg.provider_metadata, None);
    assert_eq!(msg.tool_result_is_error, None);
    assert_eq!(msg.tool_result_duration_ms, None);
    assert_eq!(msg.created_at, None);
}

#[test]
fn test_actual_fields_read_write() {
    let mut msg = Message::user("test");
    msg.actual_tokens_input = Some(100);
    msg.actual_tokens_output = Some(200);
    msg.actual_cache_read = Some(50);
    msg.actual_cache_write = Some(30);
    msg.actual_cost = Some(0.005);
    msg.provider_metadata = Some(serde_json::json!({"model": "claude-3"}));
    msg.tool_result_is_error = Some(true);
    msg.tool_result_duration_ms = Some(1500);
    msg.created_at = Some(1700000000000);

    assert_eq!(msg.actual_tokens_input, Some(100));
    assert_eq!(msg.actual_tokens_output, Some(200));
    assert_eq!(msg.actual_cache_read, Some(50));
    assert_eq!(msg.actual_cache_write, Some(30));
    assert_eq!(msg.actual_cost, Some(0.005));
    assert_eq!(
        msg.provider_metadata,
        Some(serde_json::json!({"model": "claude-3"}))
    );
    assert_eq!(msg.tool_result_is_error, Some(true));
    assert_eq!(msg.tool_result_duration_ms, Some(1500));
    assert_eq!(msg.created_at, Some(1700000000000));
}

#[test]
fn test_actual_fields_serde() {
    let mut msg = Message::assistant("hello");
    msg.actual_tokens_input = Some(50);
    msg.actual_tokens_output = Some(100);
    msg.actual_cost = Some(0.0015);
    msg.provider_metadata = Some(serde_json::json!({"model": "gpt-4"}));

    let json = serde_json::to_string(&msg).unwrap();
    let deserialized: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.actual_tokens_input, Some(50));
    assert_eq!(deserialized.actual_tokens_output, Some(100));
    assert_eq!(deserialized.actual_cost, Some(0.0015));
    assert_eq!(
        deserialized.provider_metadata,
        Some(serde_json::json!({"model": "gpt-4"}))
    );
}

#[test]
fn test_actual_fields_backward_compat() {
    // Old JSON without new fields should deserialize successfully
    let old_json = r#"{
        "role": "user",
        "kind": "user",
        "content": "hello"
    }"#;
    let msg: Message = serde_json::from_str(old_json).unwrap();
    assert_eq!(msg.content, "hello");
    assert_eq!(msg.actual_tokens_input, None);
    assert_eq!(msg.actual_cost, None);
    assert_eq!(msg.provider_metadata, None);
}

// ── Existing token tests ──

#[test]
fn test_user_message_tokens() {
    let msg = Message::user("hello");
    assert_eq!(msg.estimated_tokens, 3); // ceil(5/4)=2 + 1 role
}

#[test]
fn test_user_empty_message_tokens() {
    let msg = Message::user("");
    assert_eq!(msg.estimated_tokens, 1); // 0 + 1 role
}

#[test]
fn test_tool_message_tokens() {
    let msg = Message::tool("output", "call_1");
    assert_eq!(msg.estimated_tokens, 5); // ceil(6/4)=2 + 1 role + ceil(6/4)=2 = 5
}

#[test]
fn test_assistant_message_tokens() {
    let msg = Message::assistant("hi");
    assert_eq!(msg.estimated_tokens, 2); // ceil(2/4)=1 + 1 role
}

#[test]
fn test_system_message_tokens() {
    let msg = Message::system("prompt");
    assert_eq!(msg.estimated_tokens, 3); // ceil(6/4)=2 + 1 role
}

#[test]
fn test_estimate_tokens_non_empty() {
    assert_eq!(estimate_tokens("test"), 1); // ceil(4/4)=1
}

#[test]
fn test_estimate_tokens_empty() {
    assert_eq!(estimate_tokens(""), 0);
}

#[test]
fn test_estimate_tokens_longer() {
    assert_eq!(estimate_tokens("hello world"), 3); // ceil(11/4)=3
}

#[test]
fn test_extract_images_local_path() {
    // 创建一个临时图片文件
    let dir = tempfile::tempdir().unwrap();
    let img_path = dir.path().join("test.png");
    std::fs::write(&img_path, b"\x89PNG fake data").unwrap();
    let path_str = img_path.to_str().unwrap();

    let text = format!("看这张图 <image: {}> 好看吗", path_str);
    let (clean, images) = Message::extract_images(&text);

    // 标记被移除，文本保留
    assert_eq!(clean, "看这张图  好看吗".to_string());
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].path, path_str);
    assert_eq!(images[0].mime_type, "image/png");
    // base64 编码正确（不含 data: 前缀）
    let expected = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"\x89PNG fake data",
    );
    assert_eq!(images[0].base64, expected);
}

#[test]
fn test_extract_images_url_marker_kept() {
    let text = "看 <image: | https://example.com/a.png> 这个";
    let (clean, images) = Message::extract_images(text);
    // URL 标记保留在文本中
    assert_eq!(clean, text);
    assert!(images.is_empty());
}

#[test]
fn test_extract_images_missing_file_skipped() {
    let text = "图 <image: /nonexistent/path.png> 没了";
    let (clean, images) = Message::extract_images(text);
    // 文件读取失败时静默跳过标记
    assert_eq!(clean, "图  没了");
    assert!(images.is_empty());
}
