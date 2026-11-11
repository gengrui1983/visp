use super::*;
use visp_core::message::ToolCallRequest;

// --- OpenAiProvider 构造测试 ---

#[test]
fn test_provider_with_base_url() {
    let provider =
        OpenAiProvider::with_base_url("test-key".into(), "https://custom.openai.com".into());
    assert_eq!(provider.api_url, "https://custom.openai.com");
}

#[test]
fn test_provider_default_url() {
    let provider = OpenAiProvider::new("test-key".into());
    assert_eq!(provider.api_url, "https://api.openai.com");
}

// --- build_openai_headers 测试 ---

#[test]
fn test_build_headers() {
    let headers = build_openai_headers("sk-test123");
    assert_eq!(
        headers.get(reqwest::header::AUTHORIZATION).unwrap(),
        "Bearer sk-test123"
    );
    assert_eq!(
        headers.get(reqwest::header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(
        headers.get(reqwest::header::USER_AGENT).unwrap(),
        "visp/0.1.0"
    );
}

// --- build_openai_messages 测试 ---

#[test]
fn test_build_messages_simple() {
    let msgs = vec![
        Message::system("You are a helpful assistant."),
        Message::user("Hello!"),
    ];
    let result = build_openai_messages(&msgs);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0]["role"], "system");
    assert_eq!(result[0]["content"], "You are a helpful assistant.");
    assert_eq!(result[1]["role"], "user");
    assert_eq!(result[1]["content"], "Hello!");
}

#[test]
fn test_build_messages_mid_conversation_system_becomes_user() {
    // 上下文裁剪 / doom-loop 守卫会在历史中间插入 system 消息;
    // Qwen 等模型的 chat template 只允许 system 在开头,否则 400
    // "System message must be at the beginning."
    let msgs = vec![
        Message::system("You are a helpful assistant."),
        Message::user("Find the bug."),
        Message::system("[... earlier messages omitted due to context limit ...]"),
        Message::assistant("Looking."),
        Message::system("You appear to be repeating the same tool calls."),
    ];
    let result = build_openai_messages(&msgs);
    assert_eq!(result.len(), 5);
    assert_eq!(result[0]["role"], "system");
    assert_eq!(result[0]["content"], "You are a helpful assistant.");
    for (i, m) in result.iter().enumerate().skip(1) {
        assert_ne!(m["role"], "system", "message {i} must not be system");
    }
    assert_eq!(result[2]["role"], "user");
    assert_eq!(
        result[2]["content"],
        "[System notice] [... earlier messages omitted due to context limit ...]"
    );
    assert_eq!(result[4]["role"], "user");
    assert_eq!(
        result[4]["content"],
        "[System notice] You appear to be repeating the same tool calls."
    );
}

#[test]
fn test_build_messages_leading_system_messages_kept() {
    // 开头连续的多条 system 仍保持 system(位置合法)
    let msgs = vec![
        Message::system("A"),
        Message::system("B"),
        Message::user("hi"),
    ];
    let result = build_openai_messages(&msgs);
    assert_eq!(result[0]["role"], "system");
    assert_eq!(result[1]["role"], "system");
    assert_eq!(result[2]["role"], "user");
}

#[test]
fn test_build_openai_messages_with_images() {
    use visp_core::message::ImageData;
    let mut msg = Message::user("这是什么？");
    msg.images = vec![ImageData {
        path: "/tmp/test.png".to_string(),
        base64: "iVBORw0KGgo=".to_string(),
        mime_type: "image/png".to_string(),
    }];
    let messages = build_openai_messages(&[msg]);
    let user_msg = &messages[0];
    assert_eq!(user_msg["role"], "user");
    let content = user_msg["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "这是什么？");
    assert_eq!(content[1]["type"], "image_url");
    assert!(
        content[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[test]
fn test_build_messages_with_tool_result() {
    let msgs = vec![
        Message::user("Read the file."),
        Message::tool_call(vec![ToolCallRequest {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"test.txt"}"#.into(),
        }]),
        Message::tool("file content", "call_1"),
    ];
    let result = build_openai_messages(&msgs);
    assert_eq!(result.len(), 3);
    assert_eq!(result[1]["role"], "assistant");
    assert_eq!(result[2]["role"], "tool");
    assert_eq!(result[2]["tool_call_id"], "call_1");
    assert_eq!(result[2]["content"], "file content");
}

#[test]
fn test_build_messages_with_extra_blocks() {
    let msgs = vec![
        Message::user("Think step by step"),
        Message {
            role: Role::Assistant,
            content: "Let me think".into(),
            kind: visp_core::message::MessageType::Text,
            tool_calls: None,
            tool_call_id: None,
            tool_call_count: None,
            extra_blocks: Some(vec![serde_json::json!({
                "type": "thinking",
                "thinking": "I need to reason about this",
                "signature": "sig_123",
            })]),
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
        },
    ];
    let result = build_openai_messages(&msgs);
    assert_eq!(result.len(), 2);
    let assistant = &result[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "Let me think");
    // thinking block 映射为 reasoning_content（DeepSeek 要求）
    assert_eq!(
        assistant["reasoning_content"],
        "I need to reason about this"
    );
    assert!(assistant.get("thinking").is_none());
    // "type" 字段应被跳过，不出现在消息中
    assert!(assistant.get("type").is_none());
}

#[test]
fn test_build_messages_thinking_only() {
    // 纯 thinking 消息：content 应为空字符串，reasoning 写入 reasoning_content
    let mut msg = Message::thinking("Let me think carefully");
    msg.extra_blocks = Some(vec![serde_json::json!({
        "type": "thinking",
        "thinking": "Let me think carefully",
    })]);
    let msgs = vec![Message::user("Solve this"), msg];
    let result = build_openai_messages(&msgs);
    assert_eq!(result.len(), 2);
    let assistant = &result[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "");
    assert_eq!(assistant["reasoning_content"], "Let me think carefully");
    assert!(assistant.get("thinking").is_none());
}

#[test]
fn test_build_messages_thinking_only_without_blocks() {
    // 纯 thinking 消息但没有 extra_blocks（退化情况）：content 仍为空字符串
    let msgs = vec![
        Message::user("Solve this"),
        Message::thinking("Thinking..."),
    ];
    let result = build_openai_messages(&msgs);
    let assistant = &result[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "");
    assert!(assistant.get("reasoning_content").is_none());
}

// --- ReasoningEchoPolicy 测试 ---

#[test]
fn test_fill_missing_reasoning_mixed_history() {
    // 混合历史：1 条带 reasoning + 18 条不带（对应真实重放中 19 条 assistant
    // 的混合历史）。FillMissing 后 19/19 全带 reasoning_content，缺失者补 ""，
    // 原有 reasoning 文本不被覆盖。
    let mut msgs: Vec<Message> = Vec::new();
    msgs.push(Message::user("你好"));
    for i in 0..18 {
        msgs.push(Message::assistant(format!("无 reasoning 的回复 {i}")));
    }
    let mut with_reasoning = Message::assistant("带 reasoning 的回复");
    with_reasoning.extra_blocks = Some(vec![serde_json::json!({
        "type": "thinking",
        "thinking": "原始思考文本",
    })]);
    msgs.push(with_reasoning);

    let result = build_openai_messages_with_policy(&msgs, ReasoningEchoPolicy::FillMissing);
    assert_eq!(result.len(), 20); // 1 user + 19 assistant
    // 19 条 assistant 消息全部带 reasoning_content
    for (i, m) in result.iter().enumerate().skip(1) {
        assert_eq!(m["role"], "assistant");
        assert!(
            m.get("reasoning_content").is_some(),
            "assistant 消息 {i} 应补齐 reasoning_content"
        );
    }
    // 缺失者补 ""，原有 reasoning 文本不被覆盖
    for m in result.iter().take(19).skip(1) {
        assert_eq!(m["reasoning_content"], "");
    }
    assert_eq!(result[19]["reasoning_content"], "原始思考文本");
}

#[test]
fn test_fill_missing_reasoning_none_present() {
    // 整批 assistant 都没有 reasoning_content → 一条都不新增该字段
    let msgs = vec![
        Message::user("你好"),
        Message::assistant("回复一"),
        Message::assistant("回复二"),
    ];
    let result = build_openai_messages_with_policy(&msgs, ReasoningEchoPolicy::FillMissing);
    for m in &result {
        assert!(
            !m.as_object().unwrap().contains_key("reasoning_content"),
            "不应新增 reasoning_content: {m}"
        );
    }
}

#[test]
fn test_fill_missing_reasoning_ignores_other_roles() {
    // system/user/tool 消息不被动（即使存在带 reasoning 的 assistant）
    let mut with_reasoning = Message::assistant("带 reasoning 的回复");
    with_reasoning.extra_blocks = Some(vec![serde_json::json!({
        "type": "thinking",
        "thinking": "思考",
    })]);
    let msgs = vec![
        Message::system("你是助手"),
        Message::user("你好"),
        with_reasoning,
        Message::tool_call(vec![ToolCallRequest {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]),
        Message::tool("文件内容", "call_1"),
    ];
    let result = build_openai_messages_with_policy(&msgs, ReasoningEchoPolicy::FillMissing);
    assert_eq!(result[0]["role"], "system");
    assert!(
        !result[0]
            .as_object()
            .unwrap()
            .contains_key("reasoning_content"),
        "system 消息不应被补 reasoning_content"
    );
    assert_eq!(result[1]["role"], "user");
    assert!(
        !result[1]
            .as_object()
            .unwrap()
            .contains_key("reasoning_content"),
        "user 消息不应被补 reasoning_content"
    );
    assert_eq!(result[4]["role"], "tool");
    assert!(
        !result[4]
            .as_object()
            .unwrap()
            .contains_key("reasoning_content"),
        "tool 消息不应被补 reasoning_content"
    );
    // assistant 角色（含 tool_call 消息）应补齐
    assert_eq!(result[2]["reasoning_content"], "思考");
    assert_eq!(result[3]["role"], "assistant");
    assert_eq!(result[3]["reasoning_content"], "");
}

#[test]
fn test_build_messages_passthrough_mixed_history() {
    // Passthrough（默认 build_openai_messages）：混合历史输出与改动前一致，
    // 缺失 reasoning_content 的 assistant 消息仍无该字段
    let mut with_reasoning = Message::assistant("带 reasoning 的回复");
    with_reasoning.extra_blocks = Some(vec![serde_json::json!({
        "type": "thinking",
        "thinking": "思考文本",
    })]);
    let msgs = vec![
        Message::user("你好"),
        Message::assistant("无 reasoning 的回复"),
        with_reasoning,
    ];
    let result = build_openai_messages(&msgs);
    assert!(result[1].get("reasoning_content").is_none());
    assert_eq!(result[2]["reasoning_content"], "思考文本");
}

#[test]
fn test_build_openai_request_with_policy_fill_missing() {
    // build_openai_request_with_policy(..., FillMissing) 正确透传到 messages
    let mut with_reasoning = Message::assistant("带 reasoning 的回复");
    with_reasoning.extra_blocks = Some(vec![serde_json::json!({
        "type": "thinking",
        "thinking": "思考文本",
    })]);
    let msgs = vec![
        Message::user("你好"),
        Message::assistant("无 reasoning 的回复"),
        with_reasoning,
    ];
    let config = LlmConfig::default();
    let request =
        build_openai_request_with_policy(&msgs, &[], &config, ReasoningEchoPolicy::FillMissing);
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages[1]["reasoning_content"], "");
    assert_eq!(messages[2]["reasoning_content"], "思考文本");
    // 其余请求字段不受策略影响
    assert_eq!(request["model"], config.model);
    assert!(request["stream"].as_bool().unwrap());
}

#[test]
fn test_is_reasoning_echo_error() {
    // 命中真实上游报文样例 → true
    let real = r#"{"error":{"type":"invalid_request_error","code":"invalid_request_error","message":"Error from provider (Console Go): Upstream request failed: [invalid_request_error] The `reasoning_content` in the thinking mode must be passed back to the API."}}"#;
    assert!(is_reasoning_echo_error(400, real));
    // 大小写不敏感
    assert!(is_reasoning_echo_error(400, &real.to_uppercase()));
    // 400 但其它报文 → false
    assert!(!is_reasoning_echo_error(
        400,
        r#"{"error":{"message":"bad request"}}"#
    ));
    // 只含其中一个关键词 → false
    assert!(!is_reasoning_echo_error(400, "reasoning_content missing"));
    assert!(!is_reasoning_echo_error(400, "must be passed back"));
    // 200 / 其它状态码 → false
    assert!(!is_reasoning_echo_error(200, real));
    assert!(!is_reasoning_echo_error(500, real));
}

#[test]
fn test_strip_reasoning_content() {
    // 移除所有 assistant 消息的 reasoning_content，其它字段与消息顺序不变
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "sys"}),
        serde_json::json!({"role": "user", "content": "hi"}),
        serde_json::json!({"role": "assistant", "content": "a", "reasoning_content": "思考"}),
        serde_json::json!({"role": "assistant", "content": "b"}),
        serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "call_1", "type": "function"}],
            "reasoning_content": "思考2"
        }),
        serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "res"}),
    ];
    strip_reasoning_content(&mut messages);
    // 所有 reasoning_content 被移除
    for m in &messages {
        assert!(
            !m.as_object().unwrap().contains_key("reasoning_content"),
            "reasoning_content 应被移除: {m}"
        );
    }
    // 其它字段与顺序不变
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "sys");
    assert_eq!(messages[2]["content"], "a");
    assert_eq!(messages[3]["content"], "b");
    assert_eq!(messages[4]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[5]["role"], "tool");
    assert_eq!(messages[5]["content"], "res");
}

#[test]
fn test_extra_blocks_does_not_overwrite_reserved_fields() {
    let msgs = vec![
        Message::user("Try to override fields"),
        Message {
            role: Role::Assistant,
            content: "Original content".into(),
            kind: visp_core::message::MessageType::ToolCall,
            tool_calls: Some(vec![ToolCallRequest {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }]),
            tool_call_id: None,
            tool_call_count: None,
            extra_blocks: Some(vec![serde_json::json!({
                "role": "user",
                "content": "malicious content",
                "tool_calls": "should not appear",
                "name": "should not appear",
                "thinking": "this is fine",
            })]),
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
        },
    ];
    let result = build_openai_messages(&msgs);
    let assistant = &result[1];
    // 保留字段不能被覆盖
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "Original content");
    assert!(assistant["tool_calls"].is_array());
    // 非保留字段应被合并
    assert_eq!(assistant["thinking"], "this is fine");
}

// --- build_openai_request 测试 ---

#[test]
fn test_build_request_basic() {
    let msgs = vec![Message::user("Hi")];
    let config = LlmConfig {
        model: "gpt-4o".into(),
        temperature: 0.5,
        max_tokens: 100,
        ..Default::default()
    };
    let req = build_openai_request(&msgs, &[], &config);
    assert_eq!(req["model"], "gpt-4o");
    assert_eq!(req["temperature"], 0.5);
    assert_eq!(req["max_tokens"], 100);
    assert!(req["stream"].as_bool().unwrap());
    assert_eq!(req["stream_options"]["include_usage"].as_bool(), Some(true));
    assert_eq!(req["messages"][0]["content"], "Hi");
}

#[test]
fn test_build_request_with_tools() {
    let msgs = vec![Message::user("List files")];
    let tools = vec![ToolDefinition {
        name: "list_files".into(),
        description: "List files in directory".into(),
        category: "files".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            }
        }),
    }];
    let config = LlmConfig::default();
    let req = build_openai_request(&msgs, &tools, &config);
    assert!(req["tools"].is_array());
    assert_eq!(req["tools"][0]["type"], "function");
    assert_eq!(req["tools"][0]["function"]["name"], "list_files");
}

#[test]
fn test_build_request_tool_choice_string() {
    let msgs = vec![Message::user("List files")];
    let tools = vec![ToolDefinition {
        name: "list_files".into(),
        description: "List files in directory".into(),
        category: "files".into(),
        parameters: serde_json::json!({ "type": "object" }),
    }];
    let mut config = LlmConfig::default();
    config.extra.insert("tool_choice".into(), "required".into());
    let req = build_openai_request(&msgs, &tools, &config);
    assert_eq!(req["tool_choice"], "required");
}

#[test]
fn test_build_request_tool_choice_json() {
    let msgs = vec![Message::user("List files")];
    let tools = vec![ToolDefinition {
        name: "list_files".into(),
        description: "List files in directory".into(),
        category: "files".into(),
        parameters: serde_json::json!({ "type": "object" }),
    }];
    let mut config = LlmConfig::default();
    config.extra.insert(
        "tool_choice".into(),
        r#"{"type":"function","function":{"name":"list_files"}}"#.into(),
    );
    let req = build_openai_request(&msgs, &tools, &config);
    assert_eq!(req["tool_choice"]["type"], "function");
    assert_eq!(req["tool_choice"]["function"]["name"], "list_files");
}

#[test]
fn test_build_request_tool_choice_auto() {
    let msgs = vec![Message::user("Hi")];
    let tools = vec![];
    let mut config = LlmConfig::default();
    config.extra.insert("tool_choice".into(), "auto".into());
    let req = build_openai_request(&msgs, &tools, &config);
    // 即使没有 tools，tool_choice 也应透传
    assert_eq!(req["tool_choice"], "auto");
}

// --- enable_thinking 透传测试 ---

#[test]
fn test_build_request_enable_thinking_true() {
    let msgs = vec![Message::user("Hi")];
    let mut config = LlmConfig::default();
    config.extra.insert("enable_thinking".into(), "true".into());
    let req = build_openai_request(&msgs, &[], &config);
    assert_eq!(req["enable_thinking"], true);
}

#[test]
fn test_build_request_enable_thinking_false() {
    let msgs = vec![Message::user("Hi")];
    let mut config = LlmConfig::default();
    config
        .extra
        .insert("enable_thinking".into(), "false".into());
    let req = build_openai_request(&msgs, &[], &config);
    assert_eq!(req["enable_thinking"], false);
}

#[test]
fn test_build_request_no_enable_thinking() {
    // extra 中无该键时，请求体不应包含该字段（向后兼容）
    let msgs = vec![Message::user("Hi")];
    let config = LlmConfig::default();
    let req = build_openai_request(&msgs, &[], &config);
    assert!(req.get("enable_thinking").is_none());
}

#[test]
fn test_build_request_invalid_enable_thinking() {
    // 非法值应被忽略，不写入请求体
    let msgs = vec![Message::user("Hi")];
    let mut config = LlmConfig::default();
    config.extra.insert("enable_thinking".into(), "yes".into());
    let req = build_openai_request(&msgs, &[], &config);
    assert!(req.get("enable_thinking").is_none());
}

// --- parse_openai_sse_data 测试 ---

#[test]
fn test_parse_text_delta() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::TextDelta(t) => assert_eq!(t, "Hello"),
        _ => panic!("expected TextDelta, got {:?}", events),
    }
}

#[test]
fn test_parse_tool_call_start() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ToolCallStart { id, name, .. } => {
            assert_eq!(id, "call_abc");
            assert_eq!(name, "read_file");
        }
        _ => panic!("expected ToolCallStart, got {:?}", events),
    }
}

#[test]
fn test_parse_tool_call_delta() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"te"}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ToolCallDelta { arguments, .. } => {
            assert_eq!(arguments, "{\"path\":\"te");
        }
        _ => panic!("expected ToolCallDelta, got {:?}", events),
    }
}

#[test]
fn test_parse_finish_stop() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::Finish {
            reason: Some(r), ..
        } => assert_eq!(r, "stop"),
        _ => panic!("expected Finish, got {:?}", events),
    }
}

#[test]
fn test_parse_finish_tool_calls() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::Finish {
            reason: Some(r), ..
        } => assert_eq!(r, "tool_calls"),
        _ => panic!("expected Finish, got {:?}", events),
    }
}

#[test]
fn test_parse_done_marker() {
    let events = parse_openai_sse_data("[DONE]").unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], OpenAiStreamEvent::StreamEnd));
}

#[test]
fn test_parse_usage() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, 10);
            assert_eq!(*output_tokens, 20);
            assert_eq!(*cache_creation_input_tokens, 0);
            assert_eq!(*cache_read_input_tokens, 0);
        }
        _ => panic!("expected Usage, got {:?}", events),
    }
}

#[test]
fn test_parse_null_usage_skipped() {
    // Some providers (e.g. Ark/volcengine) send "usage": null in every chunk
    // until the final usage-only chunk. null usage should NOT produce a Usage event.
    let data = r#"{"id":"1","choices":[{"delta":{"content":"hi"},"index":0}],"usage":null}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::TextDelta(text) => assert_eq!(text, "hi"),
        _ => panic!("expected TextDelta, got {:?}", events),
    }
}

#[test]
fn test_parse_usage_with_cache() {
    // OpenAI 标准格式：prompt_tokens_details.cached_tokens
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":40}}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            cost: _,
        } => {
            assert_eq!(*input_tokens, 100);
            assert_eq!(*output_tokens, 50);
            assert_eq!(*cache_creation_input_tokens, 0);
            assert_eq!(*cache_read_input_tokens, 40);
        }
        _ => panic!("expected Usage, got {:?}", events),
    }
}

// --- parse_retry_after 测试 ---

#[test]
fn test_parse_retry_after_valid() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
    assert_eq!(crate::util::parse_retry_after(&headers), Some(30));
}

#[test]
fn test_parse_retry_after_missing() {
    let headers = reqwest::header::HeaderMap::new();
    assert_eq!(crate::util::parse_retry_after(&headers), None);
}

#[test]
fn test_parse_reasoning_content_delta() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"Step 1: think"},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ReasoningDelta(t) => assert_eq!(t, "Step 1: think"),
        _ => panic!("expected ReasoningDelta, got {:?}", events),
    }
}

#[test]
fn test_parse_reasoning_field_delta() {
    // Some providers use "reasoning" instead of "reasoning_content"
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":"deep thinking..."},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ReasoningDelta(t) => assert_eq!(t, "deep thinking..."),
        _ => panic!("expected ReasoningDelta, got {:?}", events),
    }
}

/// Regression: the opencode zen gateway attaches an incremental `usage` object
/// to EVERY chunk (violating the OpenAI convention where only the final chunk
/// carries usage and has empty choices). The parser must not let the usage
/// branch swallow the delta content in the same chunk.
#[test]
fn test_parse_usage_does_not_swallow_content_delta() {
    let data = r#"{"id":"router-1","object":"chat.completion.chunk","choices":[{"index":0,"finish_reason":null,"delta":{"content":"我是"}}],"usage":{"prompt_tokens":87,"completion_tokens":27,"total_tokens":114,"prompt_tokens_details":{}}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::TextDelta(t) if t == "我是")),
        "content delta must be emitted even when chunk carries usage, got: {:?}",
        events
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OpenAiStreamEvent::Usage {
                output_tokens: 27,
                ..
            }
        )),
        "usage must still be recorded, got: {:?}",
        events
    );
}

/// Same regression for reasoning_content deltas.
#[test]
fn test_parse_usage_does_not_swallow_reasoning_delta() {
    let data = r#"{"id":"router-1","object":"chat.completion.chunk","choices":[{"index":0,"finish_reason":null,"delta":{"reasoning_content":"我们"}}],"usage":{"prompt_tokens":87,"completion_tokens":1,"total_tokens":88,"prompt_tokens_details":{}}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::ReasoningDelta(t) if t == "我们")),
        "reasoning delta must be emitted even when chunk carries usage, got: {:?}",
        events
    );
}

/// Standard-convention chunks (empty choices + usage) must still yield Usage.
#[test]
fn test_parse_usage_with_empty_choices_still_works() {
    let data = r#"{"id":"router-1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":87,"completion_tokens":34,"total_tokens":121}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        OpenAiStreamEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, 87);
            assert_eq!(*output_tokens, 34);
        }
        _ => panic!("expected Usage, got {:?}", events),
    }
}

// --- 图片内容块解析测试 ---

/// OpenAI 图片输出：delta.content 为数组，含 data URI 图片
#[test]
fn test_parse_image_base64() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ImageBlock {
            data,
            mime_type,
            remote_url,
        } => {
            assert_eq!(data.as_deref(), Some("iVBORw0KGgo="));
            assert_eq!(mime_type, "image/png");
            assert!(remote_url.is_none());
        }
        _ => panic!("expected ImageBlock, got {:?}", events),
    }
}

/// OpenAI 图片输出：delta.content 为数组，含远程 URL 图片
#[test]
fn test_parse_image_url() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::ImageBlock {
            data,
            mime_type,
            remote_url,
        } => {
            assert!(data.is_none());
            assert!(mime_type.is_empty());
            assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
        }
        _ => panic!("expected ImageBlock, got {:?}", events),
    }
}

/// OpenAI 图片输出：content 数组同时包含文本和图片 -> 返回 TextDelta + ImageBlock
#[test]
fn test_parse_mixed_content() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":[{"type":"text","text":"Here is an image:"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 2, "expected TextDelta + ImageBlock");
    match &events[0] {
        OpenAiStreamEvent::TextDelta(t) => assert_eq!(t, "Here is an image:"),
        other => panic!("expected TextDelta first, got {:?}", other),
    }
    match &events[1] {
        OpenAiStreamEvent::ImageBlock {
            data,
            mime_type,
            remote_url,
        } => {
            assert_eq!(data.as_deref(), Some("iVBORw0KGgo="));
            assert_eq!(mime_type, "image/png");
            assert!(remote_url.is_none());
        }
        other => panic!("expected ImageBlock second, got {:?}", other),
    }
}

/// 向后兼容：delta.content 为字符串时仍返回单个 TextDelta
#[test]
fn test_parse_string_content_backward_compat() {
    let data = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"plain text"},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert_eq!(events.len(), 1, "expected exactly one event");
    match &events[0] {
        OpenAiStreamEvent::TextDelta(t) => assert_eq!(t, "plain text"),
        _ => panic!("expected TextDelta, got {:?}", events),
    }
}

// --- 缺口修复：按 chunk 收集事件（F1/F2/F3）---

/// 缺口一：同一 chunk 同时含 reasoning_content 与 tool_calls 时，
/// 两种事件都必须产出（此前 reasoning 分支提前 return 吞掉 tool_calls）。
#[test]
fn test_parse_reasoning_and_tool_call_same_chunk() {
    let data = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"thinking...","tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::ReasoningDelta(t) if t == "thinking...")),
        "reasoning delta must be emitted, got: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OpenAiStreamEvent::ToolCallStart { id, name, .. } if id == "call_x" && name == "read_file"
        )),
        "tool call start must also be emitted, got: {events:?}"
    );
}

/// 缺口二：单个 chunk 的 tool_calls 含多个元素时，每个元素都要产出事件。
#[test]
fn test_parse_multiple_tool_calls_single_chunk() {
    let data = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}},{"index":1,"id":"call_b","type":"function","function":{"name":"write_file","arguments":""}}]},"finish_reason":null}]}"#;
    let events = parse_openai_sse_data(data).unwrap();
    let starts: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            OpenAiStreamEvent::ToolCallStart { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![("call_a", "read_file"), ("call_b", "write_file")],
        "both tool calls must be emitted, got: {events:?}"
    );
}

/// 缺口三：旧式 function_call（无 id）要能被解析为 ToolCallStart（合成 id）+ ToolCallDelta。
#[test]
fn test_parse_legacy_function_call() {
    let start = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"function_call":{"name":"read_file","arguments":""}},"finish_reason":null}]}"#;
    let args = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"function_call":{"arguments":"{\"path\":\"a\"}"}},"finish_reason":null}]}"#;

    let start_events = parse_openai_sse_data(start).unwrap();
    assert_eq!(start_events.len(), 1, "expected one ToolCallStart");
    match &start_events[0] {
        OpenAiStreamEvent::ToolCallStart { index, id, name } => {
            assert_eq!(*index, 0);
            assert_eq!(id, "call_legacy_0");
            assert_eq!(name, "read_file");
        }
        other => panic!("expected ToolCallStart, got {other:?}"),
    }

    let arg_events = parse_openai_sse_data(args).unwrap();
    assert_eq!(arg_events.len(), 1, "expected one ToolCallDelta");
    match &arg_events[0] {
        OpenAiStreamEvent::ToolCallDelta { index, arguments } => {
            assert_eq!(*index, 0);
            assert_eq!(arguments, "{\"path\":\"a\"}");
        }
        other => panic!("expected ToolCallDelta, got {other:?}"),
    }
}

/// 回归：同一 chunk 同时含 content、reasoning、tool_calls、finish_reason 与 usage，
/// 全部事件都要产出，且 finish_reason 在 tool 事件之后处理。
#[test]
fn test_parse_all_fields_same_chunk() {
    let data = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi","reasoning":"r","tool_calls":[{"index":0,"id":"call_z","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#;
    let events = parse_openai_sse_data(data).unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::TextDelta(t) if t == "hi"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::ReasoningDelta(t) if t == "r"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OpenAiStreamEvent::ToolCallStart { id, .. } if id == "call_z"))
    );
    assert!(events.iter().any(
        |e| matches!(e, OpenAiStreamEvent::ToolCallDelta { arguments, .. } if arguments == "{}")
    ));
    assert!(
        events.iter().any(|e| matches!(
            e,
            OpenAiStreamEvent::Usage {
                output_tokens: 2,
                ..
            }
        )),
        "usage must still be recorded, got: {events:?}"
    );
    let finish_pos = events
        .iter()
        .position(|e| matches!(e, OpenAiStreamEvent::Finish { .. }))
        .expect("finish event must be present");
    let text_pos = events
        .iter()
        .position(|e| matches!(e, OpenAiStreamEvent::TextDelta(_)))
        .unwrap();
    assert!(
        finish_pos > text_pos,
        "finish_reason should be processed after delta fields, got: {events:?}"
    );
}

// --- byte_stream_to_chat_events 测试 ---

/// 构建单条 SSE data 行（自动追加 \n\n）
fn sse_line(data: &str) -> String {
    format!("data: {}\n\n", data)
}

/// 构建 OpenAI SSE 数据行，自动 JSON 编码
fn make_sse(val: &serde_json::Value) -> String {
    sse_line(&val.to_string())
}

/// OpenAI chunk（没有 tool_calls 时）
fn make_text_chunk(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": content },
            "finish_reason": null
        }]
    })
}

/// OpenAI chunk 带 finish_reason
fn make_stop_chunk(finish_reason: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason
        }]
    })
}

/// 收集 ChatEvent 流到 Vec
async fn collect_events(chunks: Vec<String>) -> Vec<ChatEvent> {
    collect_events_with_project_path(chunks, std::env::temp_dir().to_string_lossy().into_owned())
        .await
}

/// 收集 ChatEvent 流到 Vec，可指定图片保存的 project_path
async fn collect_events_with_project_path(
    chunks: Vec<String>,
    project_path: String,
) -> Vec<ChatEvent> {
    let byte_stream = futures::stream::iter(chunks.into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let span = tracing::Span::current();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        std::time::Instant::now(),
        span,
        "test-model".to_string(),
        project_path,
        false,
        20000,
        true,
    );
    event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await
}

/// OpenAI chunk 带图片 content 数组
fn make_image_chunk(url: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": [{"type": "image_url", "image_url": {"url": url}}] },
            "finish_reason": null
        }]
    })
}

/// 为图片测试创建唯一临时项目目录
fn temp_project_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("visp_openai_img_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 端到端：base64 图片保存到磁盘并发射 ImageBlock
#[tokio::test]
async fn test_byte_stream_base64_image() {
    // 1x1 PNG
    let tiny_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let sse = format!(
        "{}{}",
        make_sse(&make_image_chunk(&format!(
            "data:image/png;base64,{tiny_png}"
        ))),
        sse_line("[DONE]"),
    );
    let project_dir = temp_project_dir("base64");
    let project_path = project_dir.to_string_lossy().into_owned();

    let events = collect_events_with_project_path(vec![sse], project_path.clone()).await;

    assert_eq!(
        events.len(),
        4,
        "expect ImageBlock + UsageInfo + OutputMetadata + Done"
    );
    match &events[0] {
        ChatEvent::ImageBlock {
            path,
            mime_type,
            remote_url,
        } => {
            assert!(
                !path.is_empty(),
                "base64 image should be saved to a local file"
            );
            assert_eq!(mime_type, "image/png");
            assert!(remote_url.is_none());
            let saved = std::path::Path::new(path);
            assert!(saved.is_file(), "image file should exist on disk: {path}");
            assert_eq!(
                saved.parent().and_then(std::path::Path::file_name),
                Some(std::ffi::OsStr::new("images")),
                "image should be saved under .visp/images, got {path}"
            );
            assert_eq!(
                saved
                    .parent()
                    .and_then(std::path::Path::parent)
                    .and_then(std::path::Path::file_name),
                Some(std::ffi::OsStr::new(".visp")),
                "image should be saved under a project's .visp directory, got {path}"
            );
        }
        other => panic!("expected ImageBlock, got {:?}", other),
    }
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// 端到端：远程 URL 图片不下载，直接透传 ImageBlock
#[tokio::test]
async fn test_byte_stream_url_image() {
    let sse = format!(
        "{}{}",
        make_sse(&make_image_chunk("https://example.com/image.png")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert_eq!(
        events.len(),
        4,
        "expect ImageBlock + UsageInfo + OutputMetadata + Done"
    );
    match &events[0] {
        ChatEvent::ImageBlock {
            path,
            mime_type,
            remote_url,
        } => {
            assert!(path.is_empty(), "URL image should not be saved locally");
            assert!(mime_type.is_empty());
            assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
        }
        other => panic!("expected ImageBlock, got {:?}", other),
    }
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

/// 端到端：非法 base64 图片 -> ImageError
#[tokio::test]
async fn test_byte_stream_base64_decode_error() {
    let sse = format!(
        "{}{}",
        make_sse(&make_image_chunk("data:image/png;base64,@@@invalid@@@")),
        sse_line("[DONE]"),
    );
    let project_dir = temp_project_dir("decode_err");
    let project_path = project_dir.to_string_lossy().into_owned();

    let events = collect_events_with_project_path(vec![sse], project_path).await;

    let _ = std::fs::remove_dir_all(&project_dir);

    assert_eq!(
        events.len(),
        4,
        "expect ImageError + UsageInfo + OutputMetadata + Done"
    );
    match &events[0] {
        ChatEvent::ImageError { reason } => {
            assert!(
                reason.contains("base64"),
                "expected base64 decode error, got: {reason}"
            );
        }
        other => panic!("expected ImageError, got {:?}", other),
    }
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_single_text_then_done() {
    let sse = format!(
        "{}{}",
        make_sse(&make_text_chunk("Hello")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert_eq!(
        events.len(),
        4,
        "expect TextDelta + UsageInfo + OutputMetadata + Done"
    );
    assert!(matches!(&events[0], ChatEvent::TextDelta(t) if t == "Hello"));
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_multiple_text_deltas() {
    let sse = format!(
        "{}{}{}",
        make_sse(&make_text_chunk("Hello")),
        make_sse(&make_text_chunk(" World")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert_eq!(
        events.len(),
        5,
        "expect TextDelta x2 + UsageInfo + OutputMetadata + Done"
    );
    assert!(matches!(&events[0], ChatEvent::TextDelta(t) if t == "Hello"));
    assert!(matches!(&events[1], ChatEvent::TextDelta(t) if t == " World"));
    assert!(matches!(&events[2], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[3], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[4], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_tool_call() {
    // 手动构建 JSON 避免 r# 在 json! 宏中的解析问题
    let tool_start = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": ""
                    }
                }]
            },
            "finish_reason": null
        }]
    });
    let tool_arg = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": "{\"path\":\"test.txt\"}" }
                }]
            },
            "finish_reason": null
        }]
    });

    let sse = format!(
        "{}{}{}{}",
        make_sse(&tool_start),
        make_sse(&tool_arg),
        make_sse(&make_stop_chunk("tool_calls")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert_eq!(
        events.len(),
        4,
        "expect ToolCall + UsageInfo + OutputMetadata + Done"
    );
    match &events[0] {
        ChatEvent::ToolCall {
            id,
            name,
            arguments,
        } => {
            assert_eq!(id, "call_abc");
            assert_eq!(name, "read_file");
            assert!(
                arguments.contains("path"),
                "arguments should contain 'path', got: {arguments}",
            );
        }
        _ => panic!("expected ToolCall, got {:?}", events[0]),
    }
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

/// 端到端（缺口一）：首帧同时含 reasoning_content 与 tool_calls，
/// 后续帧补参数 —— 最终必须解析出 ChatEvent::ToolCall。
#[tokio::test]
async fn test_byte_stream_reasoning_and_tool_call_same_chunk() {
    let first = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "reasoning_content": "let me think...",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_abc",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "" }
                }]
            },
            "finish_reason": null
        }]
    });
    let arg = make_tool_delta_chunk(0, "{\"path\":\"test.txt\"}");
    let sse = format!(
        "{}{}{}{}",
        make_sse(&first),
        make_sse(&arg),
        make_sse(&make_stop_chunk("tool_calls")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, ChatEvent::ThinkingBlock(_))),
        "reasoning must still be surfaced, got: {events:?}"
    );
    let tool = events.iter().find_map(|e| match e {
        ChatEvent::ToolCall {
            name, arguments, ..
        } => Some((name.clone(), arguments.clone())),
        _ => None,
    });
    let (name, arguments) =
        tool.expect("tool call must be parsed from same-chunk reasoning+tool_calls chunk");
    assert_eq!(name, "read_file");
    assert!(arguments.contains("test.txt"), "got: {arguments}");
}

#[tokio::test]
async fn test_byte_stream_natural_end() {
    // 没有 [DONE] 标记，流自然结束
    let sse = make_sse(&make_text_chunk("Hello"));
    let events = collect_events(vec![sse]).await;

    assert_eq!(
        events.len(),
        4,
        "expect TextDelta + UsageInfo + OutputMetadata + Done"
    );
    assert!(matches!(&events[0], ChatEvent::TextDelta(t) if t == "Hello"));
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_chunk_boundary() {
    // SSE 消息正文被拆在两个 HTTP chunk 中，`"Hel"` 和 `lo"}` 分属不同 chunk
    let part1 = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel";
    let part2 = "lo\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n".to_string();
    let events = collect_events(vec![part1.to_string(), part2]).await;

    assert_eq!(
        events.len(),
        4,
        "expect TextDelta + UsageInfo + OutputMetadata + Done"
    );
    assert!(matches!(&events[0], ChatEvent::TextDelta(t) if t == "Hello"));
    assert!(matches!(&events[1], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[2], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[3], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_empty_stream() {
    let events = collect_events(vec![]).await;

    assert_eq!(events.len(), 3, "expect UsageInfo + OutputMetadata + Done");
    assert!(matches!(&events[0], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[1], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[2], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_reasoning_then_text() {
    // reasoning_content chunks come first, then text, then stop
    let reasoning1 = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "reasoning_content": "Step 1... " },
            "finish_reason": null
        }]
    });
    let reasoning2 = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "reasoning_content": "Step 2..." },
            "finish_reason": null
        }]
    });
    let sse = format!(
        "{}{}{}{}",
        make_sse(&reasoning1),
        make_sse(&reasoning2),
        make_sse(&make_text_chunk("The answer is 42.")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    // Expect: ThinkingBlock (step1) + ThinkingBlock (step1+step2) + TextDelta + UsageInfo + OutputMetadata + Done
    assert_eq!(
        events.len(),
        6,
        "expect 2 ThinkingBlocks + TextDelta + UsageInfo + OutputMetadata + Done in streaming mode"
    );
    match &events[0] {
        ChatEvent::ThinkingBlock(block) => {
            assert_eq!(block["type"], "thinking");
            assert_eq!(block["thinking"], "Step 1... ");
        }
        _ => panic!("expected first ThinkingBlock, got {:?}", events[0]),
    }
    match &events[1] {
        ChatEvent::ThinkingBlock(block) => {
            assert_eq!(block["type"], "thinking");
            assert_eq!(block["thinking"], "Step 1... Step 2...");
        }
        _ => panic!("expected second ThinkingBlock, got {:?}", events[1]),
    }
    assert!(matches!(&events[2], ChatEvent::TextDelta(t) if t == "The answer is 42."));
    assert!(matches!(&events[3], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[4], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[5], ChatEvent::Done));
}

#[tokio::test]
async fn test_byte_stream_reasoning_only() {
    // Model outputs ONLY reasoning content, no text (the reported bug scenario)
    let reasoning = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "reasoning_content": "All tokens spent on reasoning..." },
            "finish_reason": null
        }]
    });
    // Include usage to simulate real API behavior with token counts
    let usage_chunk = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 4096,
            "total_tokens": 4196
        }
    });
    let sse = format!(
        "{}{}{}",
        make_sse(&reasoning),
        make_sse(&usage_chunk),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    // Expect: ThinkingBlock + UsageDelta + UsageInfo + OutputMetadata + Done
    // (usage chunk 携带非零增量 -> 先发 UsageDelta 供实时速率展示)
    assert_eq!(
        events.len(),
        5,
        "expect ThinkingBlock + UsageDelta + UsageInfo + OutputMetadata + Done"
    );
    match &events[0] {
        ChatEvent::ThinkingBlock(block) => {
            assert_eq!(block["type"], "thinking");
            assert_eq!(block["thinking"], "All tokens spent on reasoning...");
        }
        _ => panic!("expected ThinkingBlock, got {:?}", events[0]),
    }
    match &events[1] {
        ChatEvent::UsageDelta {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, 100);
            assert_eq!(*output_tokens, 4096);
        }
        other => panic!("expected UsageDelta, got {other:?}"),
    }
    assert!(matches!(&events[2], ChatEvent::UsageInfo { .. }));
    assert!(matches!(&events[3], ChatEvent::OutputMetadata(_)));
    assert!(matches!(&events[4], ChatEvent::Done));
}

/// 逐 chunk 上报 UsageDelta，最终 UsageInfo 取流末的累计总量。
#[tokio::test]
async fn test_usage_delta_emitted_per_chunk_and_totals_settle() {
    let text_chunk = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": { "content": "hi" }, "finish_reason": null }]
    });
    let usage_1 = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": { "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110 }
    });
    let usage_2 = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": { "prompt_tokens": 100, "completion_tokens": 30, "total_tokens": 130 }
    });
    let sse = format!(
        "{}{}{}{}",
        make_sse(&text_chunk),
        make_sse(&usage_1),
        make_sse(&usage_2),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    let deltas: Vec<(u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::UsageDelta {
                input_tokens,
                output_tokens,
                ..
            } => Some((*input_tokens, *output_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![(100, 10), (0, 20)],
        "每个 usage chunk 的增量（累计语义下求差分）"
    );

    let usage = events.iter().find_map(|e| match e {
        ChatEvent::UsageInfo {
            input_tokens,
            output_tokens,
            ..
        } => Some((*input_tokens, *output_tokens)),
        _ => None,
    });
    assert_eq!(usage, Some((100, 30)), "结算总量取流末累计值");
}

// --- UTF-8 跨 chunk 边界测试 ---

/// 收集 ChatEvent 流，接收字节 chunk（用于测试跨 chunk 的 UTF-8 切分）
async fn collect_events_from_bytes(chunks: Vec<Vec<u8>>) -> Vec<ChatEvent> {
    let byte_stream = futures::stream::iter(chunks.into_iter().map(|b| Ok(bytes::Bytes::from(b))));
    let span = tracing::Span::current();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        std::time::Instant::now(),
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await
}

#[tokio::test]
async fn test_utf8_multibyte_split_across_chunks() {
    // 中文 "之间" = e4 b9 8b e9 97 b4 (6 字节)
    // 故意在 "之" 字中间 (e4 b9 | 8b) 切分，验证不产生 U+FFFD
    let sse = format!(
        "{}{}",
        make_sse(&make_text_chunk("之间")),
        sse_line("[DONE]"),
    );
    let full_bytes = sse.into_bytes();
    let zhishi = "之间".as_bytes(); // [e4, b9, 8b, e9, 97, b4]
    let split_offset = full_bytes
        .windows(zhishi.len())
        .position(|w| w == zhishi)
        .expect("should find 之间 in SSE");
    // 在 "之"(e4 b9 8b) 的第 2 字节后切分：e4 b9 | 8b e9 97 b4
    let cut = split_offset + 2;
    let chunk1 = full_bytes[..cut].to_vec();
    let chunk2 = full_bytes[cut..].to_vec();

    let events = collect_events_from_bytes(vec![chunk1, chunk2]).await;

    // 提取所有 TextDelta
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(text, "之间", "Chinese text should survive chunk split");
    assert!(
        !text.contains('\u{FFFD}'),
        "no U+FFFD replacement char allowed"
    );
}

#[tokio::test]
async fn test_utf8_multibyte_split_at_char_boundary() {
    // 在 "之"(3字节) 和 "间"(3字节) 之间切分，验证正常边界也工作
    let sse = format!(
        "{}{}",
        make_sse(&make_text_chunk("之间")),
        sse_line("[DONE]"),
    );
    let full_bytes = sse.into_bytes();
    let zhi = "之".as_bytes(); // [e4, b9, 8b]
    let split_offset = full_bytes
        .windows(zhi.len())
        .position(|w| w == zhi)
        .expect("should find 之 in SSE");
    let cut = split_offset + zhi.len(); // 在完整字符后切分
    let chunk1 = full_bytes[..cut].to_vec();
    let chunk2 = full_bytes[cut..].to_vec();

    let events = collect_events_from_bytes(vec![chunk1, chunk2]).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "之间");
    assert!(!text.contains('\u{FFFD}'));
}

// --- truncate_for_log 测试（回归：UTF-8 char boundary panic） ---

#[test]
fn truncate_for_log_ascii_below_limit_returns_full() {
    assert_eq!(truncate_for_log("hello", 200), "hello");
}

#[test]
fn truncate_for_log_ascii_above_limit_truncates() {
    let s = "a".repeat(300);
    let out = truncate_for_log(&s, 200);
    assert_eq!(out.chars().count(), 200);
}

#[test]
fn truncate_for_log_chinese_does_not_panic() {
    // 67 个汉字 = 201 字节，触发原 bug：bytes 198..201 在 '析' 中间
    let s = "分析".repeat(100);
    let out = truncate_for_log(&s, 200);
    // 200 字符（每个 3 字节）= 600 字节
    assert_eq!(out.chars().count(), 200);
}

#[test]
fn truncate_for_log_at_exact_char_count() {
    // 边界：字符数刚好等于上限
    let s = "中文";
    assert_eq!(truncate_for_log(s, 2), "中文");
}

#[test]
fn truncate_for_log_mixed_ascii_and_chinese() {
    let s = "abc中文def";
    let out = truncate_for_log(s, 4);
    assert_eq!(out, "abc中");
    assert_eq!(out.chars().count(), 4);
}

#[test]
fn truncate_for_log_zero_limit() {
    assert_eq!(truncate_for_log("中文", 0), "");
}

// ── Tracing / gen_ai.client.operation span tests ────────────────────────

use std::sync::{Arc, Mutex};
use tracing::Event;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: Vec<(String, String)>,
    id: u64,
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
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<(String, String)>>>,
}

impl TestLayer {
    fn new(
        spans: Arc<Mutex<Vec<CapturedSpan>>>,
        events: Arc<Mutex<Vec<(String, String)>>>,
    ) -> Self {
        Self { spans, events }
    }
}

impl<S> Layer<S> for TestLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut visitor = SpanFieldVisitor { fields: Vec::new() };
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.fields,
            id: id.into_u64(),
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
        let name = event.metadata().name().to_string();
        self.events.lock().unwrap().push((target, name));
    }
}

#[allow(clippy::type_complexity)]
fn setup_tracing() -> (
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<(String, String)>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    (spans, events)
}

fn make_guard(
    spans: &Arc<Mutex<Vec<CapturedSpan>>>,
    events: &Arc<Mutex<Vec<(String, String)>>>,
) -> tracing::subscriber::DefaultGuard {
    tracing_subscriber::registry()
        .with(TestLayer::new(spans.clone(), events.clone()))
        .set_default()
}

/// 生成一组完整的 OpenAI SSE 事件（包含 usage 和 finish_reason）
fn make_openai_complete_sse(model: &str, finish_reason: &str) -> String {
    let text_chunk = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": "Hello" },
            "finish_reason": null
        }]
    });
    let usage_chunk = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [],
        "model": model,
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150
        }
    });
    let stop_chunk = serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason
        }]
    });

    fn sse_line(data: &str) -> String {
        format!("data: {}\n\n", data)
    }

    format!(
        "{}{}{}",
        sse_line(&text_chunk.to_string()),
        sse_line(&usage_chunk.to_string()),
        sse_line(&stop_chunk.to_string()),
    )
}

#[test]
fn test_gen_ai_client_operation_span_created_openai() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "gpt-4o",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "openai",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );
    span.record("gen_ai.request.max_tokens", 4096i64);
    span.record("gen_ai.request.temperature", 0.7f64);

    drop(_guard);
    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "gen_ai.client.operation");
}

#[test]
fn test_gen_ai_request_fields_at_span_start_openai() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "gpt-4o",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "openai",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    span.record("gen_ai.system", "openai");
    span.record("gen_ai.request.max_tokens", 4096i64);
    span.record("gen_ai.request.temperature", 0.7f64);

    drop(_guard);
    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let fields = &spans[0].fields;
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.request.model" && v == "gpt-4o")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.operation.name" && v == "chat")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "visp.llm.attempt" && v == "0")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.system" && v == "openai")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.provider.name" && v == "openai")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.request.max_tokens" && v == "4096")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.request.temperature" && v == "0.7")
    );
}

#[tokio::test]
async fn test_gen_ai_usage_fields_recorded_on_completion_openai() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "gpt-4o",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "openai",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );
    span.record("gen_ai.request.max_tokens", 4096i64);
    span.record("gen_ai.request.temperature", 0.7f64);

    let sse = make_openai_complete_sse("gpt-4o", "stop");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);

    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let fields = &spans[0].fields;
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.input_tokens" && v == "100")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.output_tokens" && v == "50")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"stop\"]")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.model" && v == "gpt-4o")
    );

    // OpenAI span 不应包含 cache 字段
    assert!(
        !fields
            .iter()
            .any(|(k, _)| k == "gen_ai.usage.cache_read_input_tokens")
    );
    assert!(
        !fields
            .iter()
            .any(|(k, _)| k == "gen_ai.usage.cache_creation_input_tokens")
    );
    assert!(
        !fields
            .iter()
            .any(|(k, _)| k == "gen_ai.usage.cache_read.input_tokens")
    );
    assert!(
        !fields
            .iter()
            .any(|(k, _)| k == "gen_ai.usage.cache_creation.input_tokens")
    );
}

#[tokio::test]
async fn test_openai_client_first_token_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
    );

    let sse = make_openai_complete_sse("gpt-4o", "stop");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);
    let evts = events.lock().unwrap();
    assert!(
        evts.iter().any(|(t, _)| t == "gen_ai.client.first_token"),
        "should find first_token event"
    );
}

#[tokio::test]
async fn test_openai_client_completed_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
    );

    let sse = make_openai_complete_sse("gpt-4o", "stop");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);
    let evts = events.lock().unwrap();
    assert!(
        evts.iter().any(|(t, _)| t == "gen_ai.client.completed"),
        "should find completed event; found: {:?}",
        evts,
    );
}

#[test]
fn test_gen_ai_client_retry_event_emitted_openai() {
    let (_spans, events) = setup_tracing();
    let _guard = make_guard(&_spans, &events);

    tracing::warn!(
        target: "gen_ai.client.retry",
        reason = "rate_limit",
        "retrying LLM request"
    );

    drop(_guard);
    let evts = events.lock().unwrap();
    assert!(
        evts.iter().any(|(t, _)| t == "gen_ai.client.retry"),
        "should find retry event"
    );
}

#[test]
fn test_gen_ai_provider_name_is_openai() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let _span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.provider.name = "openai",
        gen_ai.operation.name = "chat",
    );

    drop(_guard);
    let spans = spans.lock().unwrap();
    assert!(
        spans[0]
            .fields
            .iter()
            .any(|(k, v)| k == "gen_ai.provider.name" && v == "openai")
    );
}

#[tokio::test]
async fn test_openai_finish_reason_length_stays_length() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    let sse = make_openai_complete_sse("gpt-4o", "length");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);
    let spans = spans.lock().unwrap();
    let fields = &spans[0].fields;

    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"length\"]"),
        "OpenAI length should stay as length"
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "visp.llm.token_limit_hit" && v == "true"),
        "token_limit_hit should be true for length"
    );
}

#[tokio::test]
async fn test_openai_finish_reason_stop_stays_stop() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    let sse = make_openai_complete_sse("gpt-4o", "stop");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);
    let spans = spans.lock().unwrap();
    let fields = &spans[0].fields;

    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"stop\"]")
    );
    // stop 不应设置 token_limit_hit
    let token_limit_entry = fields.iter().find(|(k, _)| k == "visp.llm.token_limit_hit");
    assert!(token_limit_entry.is_none() || token_limit_entry.unwrap().1 == "false");
}

#[tokio::test]
async fn test_openai_no_cache_fields() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
    );

    let sse = make_openai_complete_sse("gpt-4o", "stop");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        false,
        20000,
        true,
    );
    let _: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    drop(_guard);
    let spans = spans.lock().unwrap();
    let fields = &spans[0].fields;

    assert!(
        !fields
            .iter()
            .any(|(k, _)| k.starts_with("gen_ai.usage.cache")),
        "OpenAI span should not have any cache fields, got: {:?}",
        fields
    );
}

// --- 文生图（image generation）测试 ---

#[test]
fn test_build_image_generation_request_basic() {
    // Test that the image generation request body is correctly formed
    // We test the request body construction logic directly
    let config = LlmConfig {
        model: "doubao-seedream-5.0-lite".to_string(),
        image_generation: true,
        ..Default::default()
    };

    let body = serde_json::json!({
        "model": config.model,
        "prompt": "a cat",
        "response_format": "url",
    });

    assert_eq!(body["model"], "doubao-seedream-5.0-lite");
    assert_eq!(body["prompt"], "a cat");
    assert_eq!(body["response_format"], "url");
}

#[test]
fn test_build_image_generation_request_with_extra() {
    let mut extra = std::collections::HashMap::new();
    extra.insert("size".to_string(), "2K".to_string());
    extra.insert("output_format".to_string(), "png".to_string());
    extra.insert("watermark".to_string(), "false".to_string());

    let config = LlmConfig {
        model: "doubao-seedream-5.0-lite".to_string(),
        image_generation: true,
        extra,
        ..Default::default()
    };

    let mut body = serde_json::json!({
        "model": config.model,
        "prompt": "a cat",
        "response_format": "url",
    });

    for key in &["size", "output_format", "watermark"] {
        if let Some(val) = config.extra.get(*key) {
            body[key] = serde_json::Value::String(val.clone());
        }
    }

    assert_eq!(body["size"], "2K");
    assert_eq!(body["output_format"], "png");
    assert_eq!(body["watermark"], "false");
}

#[test]
fn test_image_generation_url_versioned_base() {
    // Versioned base URL (e.g. /api/plan/v3) should not add /v1 prefix
    let base = "https://ark.cn-beijing.volces.com/api/plan/v3";
    let url = if is_versioned_base_url(base) {
        format!("{}/images/generations", base)
    } else {
        format!("{}/v1/images/generations", base)
    };
    assert_eq!(
        url,
        "https://ark.cn-beijing.volces.com/api/plan/v3/images/generations"
    );
}

#[test]
fn test_image_generation_url_standard_base() {
    // Standard base URL should add /v1 prefix
    let base = "https://api.openai.com";
    let url = if is_versioned_base_url(base) {
        format!("{}/images/generations", base)
    } else {
        format!("{}/v1/images/generations", base)
    };
    assert_eq!(url, "https://api.openai.com/v1/images/generations");
}

#[test]
fn test_parse_image_generation_response() {
    // Test parsing a typical image generation API response
    let resp_json = serde_json::json!({
        "created": 1234567890,
        "data": [
            {
                "url": "https://example.com/image.png"
            }
        ]
    });

    let image_url = resp_json
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|item| item.get("url"))
        .and_then(|u| u.as_str())
        .unwrap();

    assert_eq!(image_url, "https://example.com/image.png");
}

#[test]
fn test_parse_image_generation_response_missing_data() {
    let resp_json = serde_json::json!({
        "created": 1234567890,
        "data": []
    });

    let image_url = resp_json
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|item| item.get("url"))
        .and_then(|u| u.as_str());

    assert!(image_url.is_none());
}

// --- finish_reason='length' 截断 tool_call 处理 ---

/// 构建 tool_call start 的 SSE chunk
fn make_tool_start_chunk(index: usize, id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            },
            "finish_reason": null
        }]
    })
}

/// 构建 tool_call arguments delta 的 SSE chunk
fn make_tool_delta_chunk(index: usize, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "function": { "arguments": arguments }
                }]
            },
            "finish_reason": null
        }]
    })
}

#[tokio::test]
async fn test_length_truncated_tool_call_dropped_no_text() {
    // finish_reason='length' + 畸形 arguments + 无文本 -> 提示文本，无 ToolCall
    let sse = format!(
        "{}{}{}{}",
        make_sse(&make_tool_start_chunk(0, "call_1", "read_file")),
        make_sse(&make_tool_delta_chunk(0, "{\"path\":\"te")),
        make_sse(&make_stop_chunk("length")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::ToolCall { .. })),
        "truncated tool call should be dropped, events: {:?}",
        events
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ChatEvent::TextDelta(t) if t.contains("truncated"))),
        "should emit truncation hint text, events: {:?}",
        events
    );
}

#[tokio::test]
async fn test_length_truncated_with_text_keeps_text_no_hint() {
    // finish_reason='length' + 畸形 + 有文本 -> 文本保留，无 ToolCall，无提示
    let sse = format!(
        "{}{}{}{}{}",
        make_sse(&make_text_chunk("Hello")),
        make_sse(&make_tool_start_chunk(0, "call_1", "read_file")),
        make_sse(&make_tool_delta_chunk(0, "{\"path\":\"te")),
        make_sse(&make_stop_chunk("length")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::ToolCall { .. })),
        "truncated tool call should be dropped"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ChatEvent::TextDelta(t) if t == "Hello")),
        "original text should be kept"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::TextDelta(t) if t.contains("truncated"))),
        "should NOT emit hint when text already exists"
    );
}

#[tokio::test]
async fn test_length_valid_tool_call_kept() {
    // finish_reason='length' + 合法 arguments -> 正常发射 ToolCall（不误丢）
    let sse = format!(
        "{}{}{}{}",
        make_sse(&make_tool_start_chunk(0, "call_1", "read_file")),
        make_sse(&make_tool_delta_chunk(0, "{\"path\":\"test.txt\"}")),
        make_sse(&make_stop_chunk("length")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ChatEvent::ToolCall {
                name, arguments, ..
            } = e
            {
                Some((name.clone(), arguments.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_calls.len(), 1, "valid tool call should be kept");
    assert_eq!(tool_calls[0].0, "read_file");
    assert!(tool_calls[0].1.contains("test.txt"));
}

#[tokio::test]
async fn test_stop_tool_call_normal() {
    // finish_reason='stop' + 合法 -> 正常发射（回归）
    let sse = format!(
        "{}{}{}{}",
        make_sse(&make_tool_start_chunk(0, "call_1", "read_file")),
        make_sse(&make_tool_delta_chunk(0, "{\"path\":\"test.txt\"}")),
        make_sse(&make_stop_chunk("stop")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, ChatEvent::ToolCall { name, .. } if name == "read_file")),
        "stop + valid tool call should emit ToolCall"
    );
}

#[tokio::test]
async fn test_length_partial_truncation_multi_tool() {
    // length + 多 tool（index 0 合法，index 1 畸形）-> 只发射合法的
    let sse = format!(
        "{}{}{}{}{}{}",
        make_sse(&make_tool_start_chunk(0, "call_1", "read_file")),
        make_sse(&make_tool_delta_chunk(0, "{\"path\":\"a.txt\"}")),
        make_sse(&make_tool_start_chunk(1, "call_2", "write_file")),
        make_sse(&make_tool_delta_chunk(1, "{\"path\":\"b")),
        make_sse(&make_stop_chunk("length")),
        sse_line("[DONE]"),
    );
    let events = collect_events(vec![sse]).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ChatEvent::ToolCall { id, .. } = e {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_calls.len(), 1, "only valid tool call should be kept");
    assert_eq!(
        tool_calls[0], "call_1",
        "the valid one (call_1) should remain"
    );
}

// --- build_openai_messages 防御兜底：畸形 arguments 校验 ---

#[test]
fn test_build_messages_valid_arguments_kept() {
    let msgs = vec![Message::tool_call(vec![ToolCallRequest {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"test.txt"}"#.into(),
    }])];
    let result = build_openai_messages(&msgs);
    assert_eq!(
        result[0]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"test.txt"}"#
    );
}

#[test]
fn test_build_messages_malformed_arguments_sanitized() {
    let msgs = vec![Message::tool_call(vec![ToolCallRequest {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: r#"{"path":"te"#.into(),
    }])];
    let result = build_openai_messages(&msgs);
    assert_eq!(
        result[0]["tool_calls"][0]["function"]["arguments"], "{}",
        "malformed arguments should be replaced with empty json object"
    );
}

#[test]
fn test_build_messages_mixed_arguments() {
    let msgs = vec![Message::tool_call(vec![
        ToolCallRequest {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        },
        ToolCallRequest {
            id: "call_2".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"b"#.into(),
        },
    ])];
    let result = build_openai_messages(&msgs);
    assert_eq!(
        result[0]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.txt"}"#
    );
    assert_eq!(result[0]["tool_calls"][1]["function"]["arguments"], "{}");
}

#[test]
fn test_build_messages_empty_arguments_kept() {
    let msgs = vec![Message::tool_call(vec![ToolCallRequest {
        id: "call_1".into(),
        name: "read_file".into(),
        arguments: "".into(),
    }])];
    let result = build_openai_messages(&msgs);
    assert_eq!(
        result[0]["tool_calls"][0]["function"]["arguments"], "",
        "empty arguments should be kept as-is (valid)"
    );
}

#[test]
fn test_extra_headers_applied_with_session_placeholder() {
    let provider = OpenAiProvider::with_base_url("k".into(), "https://example.com".into())
        .with_extra_headers(vec![("x-opencode-session".into(), "{session}".into())]);
    let mut headers = reqwest::header::HeaderMap::new();
    let config = LlmConfig {
        session_id: Some("sess-123".into()),
        ..Default::default()
    };
    provider.apply_extra_headers(&mut headers, &config);
    assert_eq!(
        headers.get("x-opencode-session").unwrap(),
        "sess-123",
        "placeholder {{session}} should be replaced with config.session_id"
    );
}

#[test]
fn test_extra_headers_skipped_without_session_id() {
    let provider = OpenAiProvider::with_base_url("k".into(), "https://example.com".into())
        .with_extra_headers(vec![("x-opencode-session".into(), "{session}".into())]);
    let mut headers = reqwest::header::HeaderMap::new();
    let config = LlmConfig::default();
    provider.apply_extra_headers(&mut headers, &config);
    assert!(
        headers.get("x-opencode-session").is_none(),
        "header with unresolved {{session}} placeholder should be skipped"
    );
}

#[test]
fn test_extra_headers_static_value_passes_through() {
    let provider = OpenAiProvider::with_base_url("k".into(), "https://example.com".into())
        .with_extra_headers(vec![("x-custom".into(), "static-value".into())]);
    let mut headers = reqwest::header::HeaderMap::new();
    provider.apply_extra_headers(&mut headers, &LlmConfig::default());
    assert_eq!(headers.get("x-custom").unwrap(), "static-value");
}

#[test]
fn test_extra_headers_invalid_name_skipped() {
    let provider = OpenAiProvider::with_base_url("k".into(), "https://example.com".into())
        .with_extra_headers(vec![("bad header\u{0}name".into(), "v".into())]);
    let mut headers = reqwest::header::HeaderMap::new();
    provider.apply_extra_headers(&mut headers, &LlmConfig::default());
    assert!(
        headers.is_empty(),
        "invalid header name should be skipped without panic"
    );
}
