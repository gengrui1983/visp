use super::*;

// --- AnthropicProvider 构造测试 ---

#[test]
fn test_provider_with_base_url() {
    let provider =
        AnthropicProvider::with_base_url("test-key".into(), "https://custom.api.com".into());
    assert_eq!(provider.api_url, "https://custom.api.com");
}

#[test]
fn test_provider_default_url() {
    let provider = AnthropicProvider::new("test-key".into());
    assert_eq!(provider.api_url, "https://api.anthropic.com");
}

// --- parse_anthropic_event 测试 ---

#[test]
fn test_parse_text_delta() {
    let data =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    let result = parse_anthropic_event("content_block_delta", data).unwrap();
    match result {
        ParsedEvent::Emit(ChatEvent::TextDelta(t)) => assert_eq!(t, "Hello"),
        _ => panic!("expected TextDelta, got {:?}", result),
    }
}

#[test]
fn test_parse_tool_use_start_with_empty_input() {
    // 流式模式：input 为 {}，后续由 input_json_delta 填充
    let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_123","name":"get_weather","input":{}}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    match result {
        ParsedEvent::ToolInputDelta {
            index,
            id,
            name,
            partial,
        } => {
            assert_eq!(id, "toolu_123");
            assert_eq!(name, "get_weather");
            assert_eq!(index, 0);
            assert!(
                partial.is_empty(),
                "empty input should result in empty partial"
            );
        }
        _ => panic!("expected ToolInputDelta, got {:?}", result),
    }
}

#[test]
fn test_parse_tool_use_start_with_initial_input() {
    // 非流式/大内容场景：input 直接包含完整参数
    let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_456","name":"write_file","input":{"path":"test.txt","content":"hello"}}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    match result {
        ParsedEvent::ToolInputDelta {
            index,
            id,
            name,
            partial,
        } => {
            assert_eq!(id, "toolu_456");
            assert_eq!(name, "write_file");
            assert_eq!(index, 0);
            assert!(
                !partial.is_empty(),
                "non-empty input should be preserved as partial"
            );
            // partial 应为 JSON 字符串
            let parsed: serde_json::Value = serde_json::from_str(&partial).unwrap();
            assert_eq!(parsed["path"], "test.txt");
            assert_eq!(parsed["content"], "hello");
        }
        _ => panic!("expected ToolInputDelta, got {:?}", result),
    }
}

#[test]
fn test_parse_message_stop() {
    let result = parse_anthropic_event("message_stop", r#"{"type":"message_stop"}"#).unwrap();
    match result {
        ParsedEvent::Emit(ChatEvent::Done) => {}
        _ => panic!("expected Done, got {:?}", result),
    }
}

#[test]
fn test_parse_message_start_returns_usage_and_model() {
    let data = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":52,"cache_read_input_tokens":200}}}"#;
    let result = parse_anthropic_event("message_start", data).unwrap();
    match result {
        ParsedEvent::Usage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            model,
            stop_reason,
        } => {
            assert_eq!(input_tokens, 105);
            assert_eq!(output_tokens, 0);
            assert_eq!(cache_creation_input_tokens, 52);
            assert_eq!(cache_read_input_tokens, 200);
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert_eq!(stop_reason, None);
        }
        _ => panic!("expected Usage, got {:?}", result),
    }
}

#[test]
fn test_parse_message_delta_returns_usage() {
    let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":125}}"#;
    let result = parse_anthropic_event("message_delta", data).unwrap();
    match result {
        ParsedEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(input_tokens, 0);
            assert_eq!(output_tokens, 125);
        }
        _ => panic!("expected Usage, got {:?}", result),
    }
}

#[test]
fn test_parse_unknown_event_returns_skip() {
    let result = parse_anthropic_event("ping", "{}").unwrap();
    assert!(matches!(result, ParsedEvent::Skip));
}

#[test]
fn test_parse_text_block_start_returns_skip() {
    let data =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    assert!(matches!(result, ParsedEvent::Skip));
}

#[test]
fn test_parse_redacted_thinking_block_start() {
    let data = r#"{"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","signature":"base64sig123"}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    match result {
        ParsedEvent::ThinkingDelta {
            index,
            partial,
            signature,
        } => {
            assert_eq!(index, 2);
            assert_eq!(partial, "[REDACTED]");
            assert_eq!(signature, "base64sig123");
        }
        _ => panic!("expected ThinkingDelta, got {:?}", result),
    }
}

// --- parse_retry_after 测试 ---

#[test]
fn test_parse_429_rate_limit() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_static("30"),
    );
    assert_eq!(parse_retry_after(&headers), Some(30));
}

#[test]
fn test_parse_retry_after_missing() {
    let headers = reqwest::header::HeaderMap::new();
    assert_eq!(parse_retry_after(&headers), None);
}

#[test]
fn test_parse_retry_after_invalid() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_static("not-a-number"),
    );
    assert_eq!(parse_retry_after(&headers), None);
}

#[test]
fn test_build_messages_basic() {
    let msgs = vec![Message::user("Hello"), Message::assistant("Hi there!")];
    let config = LlmConfig {
        model: "claude-sonnet-4-20250514".into(),
        temperature: 0.7,
        max_tokens: 4096,
        max_context_tokens: 128_000,
        extra: Default::default(),
        ..Default::default()
    };
    let req = build_anthropic_request(&msgs, &[], &config);
    assert_eq!(req["model"], "claude-sonnet-4-20250514");
    assert_eq!(req["max_tokens"], 4096);
    assert_eq!(req["temperature"], 0.7);
    assert!(req.get("system").is_none());

    let msgs_arr = req["messages"].as_array().unwrap();
    assert_eq!(msgs_arr.len(), 2);
    assert_eq!(msgs_arr[0]["role"], "user");
    assert_eq!(msgs_arr[0]["content"][0]["type"], "text");
    assert_eq!(msgs_arr[0]["content"][0]["text"], "Hello");
    assert_eq!(msgs_arr[1]["role"], "assistant");
    assert_eq!(msgs_arr[1]["content"][0]["text"], "Hi there!");
}

#[test]
fn test_build_anthropic_messages_with_images() {
    use visp_core::message::ImageData;
    let mut msg = Message::user("这是什么？");
    msg.images = vec![ImageData {
        path: "/tmp/test.png".to_string(),
        base64: "iVBORw0KGgo=".to_string(),
        mime_type: "image/png".to_string(),
    }];
    let messages = build_anthropic_messages(&[&msg]);
    let user_msg = &messages[0];
    assert_eq!(user_msg["role"], "user");
    let content = user_msg["content"].as_array().unwrap();
    // Should have text block + image block
    assert!(content.iter().any(|b| b["type"] == "text"));
    assert!(content.iter().any(|b| b["type"] == "image"));
    let img_block = content.iter().find(|b| b["type"] == "image").unwrap();
    assert_eq!(img_block["source"]["type"], "base64");
    assert_eq!(img_block["source"]["media_type"], "image/png");
    assert_eq!(img_block["source"]["data"], "iVBORw0KGgo=");
}

#[test]
fn test_system_message_separated() {
    let msgs = vec![
        Message::system("You are a helpful assistant."),
        Message::user("Hello"),
    ];
    let config = LlmConfig::default();
    let req = build_anthropic_request(&msgs, &[], &config);
    assert_eq!(
        req["system"],
        serde_json::json!([{
            "type": "text",
            "text": "You are a helpful assistant.",
            "cache_control": { "type": "ephemeral" }
        }])
    );
    let msgs_arr = req["messages"].as_array().unwrap();
    assert_eq!(msgs_arr.len(), 1);
    assert_eq!(msgs_arr[0]["role"], "user");
}

#[test]
fn test_tool_message_creates_new_user() {
    let msgs = vec![
        Message::user("Check the weather"),
        Message::assistant("Let me look that up"),
        Message::tool("Sunny 22°C", "toolu_abc123"),
    ];
    let config = LlmConfig::default();
    let req = build_anthropic_request(&msgs, &[], &config);
    let msgs_arr = req["messages"].as_array().unwrap();
    // tool 消息创建独立 user 消息，3 条：user, assistant, user(tool_result)
    assert_eq!(msgs_arr.len(), 3);
    assert_eq!(msgs_arr[0]["role"], "user");
    assert_eq!(msgs_arr[1]["role"], "assistant");
    assert_eq!(msgs_arr[2]["role"], "user");
    let content = msgs_arr[2]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["tool_use_id"], "toolu_abc123");
    assert_eq!(content[0]["content"], "Sunny 22°C");
}

#[test]
fn test_consecutive_same_role_merged() {
    let msgs = vec![
        Message::user("Hello"),
        Message::assistant("First response"),
        Message::assistant("Second response"),
    ];
    let config = LlmConfig::default();
    let req = build_anthropic_request(&msgs, &[], &config);
    let msgs_arr = req["messages"].as_array().unwrap();
    assert_eq!(msgs_arr.len(), 2);
    assert_eq!(msgs_arr[1]["role"], "assistant");
    let content = msgs_arr[1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], "First response\n\nSecond response");
}

// ── byte_stream_to_chat_events 集成测试 ──────────────────────────

/// 构造一条 Anthropic SSE 文本行（自动追加 \n\n）
fn sse_line(event: &str, data: &str) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

/// 收集 ChatEvent 流到 Vec（使用 Instant::now 作为 start_time）
async fn collect_anthropic_events(chunks: Vec<String>, project_path: &str) -> Vec<ChatEvent> {
    let byte_stream = futures::stream::iter(chunks.into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let span = tracing::Span::current();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        project_path.to_string(),
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
async fn test_anthropic_response_carries_metadata() {
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":125}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], "/tmp").await;

    // 提取 OutputMetadata
    let metadata_events: Vec<&ChatEvent> = events
        .iter()
        .filter(|e| matches!(e, ChatEvent::OutputMetadata(_)))
        .collect();
    assert_eq!(
        metadata_events.len(),
        1,
        "should have exactly one OutputMetadata event"
    );

    let meta = &metadata_events[0];
    if let ChatEvent::OutputMetadata(m) = meta {
        assert_eq!(m.model, "claude-sonnet-4-6");
        assert_eq!(m.finish_reasons, vec!["end_turn"]);
        assert_eq!(m.input_tokens, 105);
        assert_eq!(m.output_tokens, 125);
        assert_eq!(m.cache_read_input_tokens, None);
        assert_eq!(m.cache_creation_input_tokens, None);
        // latency_ms is u64, always non-negative
    } else {
        panic!("expected OutputMetadata");
    }

    // 验证事件顺序：UsageDelta → TextDelta → UsageDelta → UsageInfo → OutputMetadata → Done
    // （message_start / message_delta 携带非零 usage，各自先发 UsageDelta 供实时速率展示）
    let type_names: Vec<&str> = events
        .iter()
        .map(|e| match e {
            ChatEvent::TextDelta(_) => "TextDelta",
            ChatEvent::UsageDelta { .. } => "UsageDelta",
            ChatEvent::UsageInfo { .. } => "UsageInfo",
            ChatEvent::OutputMetadata(_) => "OutputMetadata",
            ChatEvent::Done => "Done",
            _ => "Other",
        })
        .collect();
    assert_eq!(
        type_names,
        vec![
            "UsageDelta",
            "TextDelta",
            "UsageDelta",
            "UsageInfo",
            "OutputMetadata",
            "Done"
        ],
        "event order should be UsageDelta → TextDelta → UsageDelta → UsageInfo → OutputMetadata → Done"
    );
}

#[tokio::test]
async fn test_anthropic_cache_tokens_extracted() {
    // message_start 包含 cache tokens
    let message_start = r#"{"type":"message_start","message":{"id":"msg_02","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200,"output_tokens":0,"cache_creation_input_tokens":80,"cache_read_input_tokens":300}}}"#;
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], "/tmp").await;

    // 提取 OutputMetadata
    let meta = events
        .iter()
        .find_map(|e| {
            if let ChatEvent::OutputMetadata(m) = e {
                Some(m.clone())
            } else {
                None
            }
        })
        .expect("should have OutputMetadata event");

    assert_eq!(meta.model, "claude-sonnet-4-5");
    assert_eq!(meta.input_tokens, 200);
    assert_eq!(meta.output_tokens, 50);
    assert_eq!(meta.cache_read_input_tokens, Some(300));
    assert_eq!(meta.cache_creation_input_tokens, Some(80));
    assert_eq!(meta.finish_reasons, vec!["end_turn"]);
}

/// 逐事件上报 UsageDelta（message_start 的 input/cache 增量 + message_delta 的
/// output 增量），最终 UsageInfo 仍取流末累计总量，不被增量重复累加。
#[tokio::test]
async fn test_anthropic_usage_delta_emitted_and_totals_settle() {
    let message_start = r#"{"type":"message_start","message":{"id":"msg_04","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":20,"cache_read_input_tokens":10}}}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":30}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}",
        sse_line("message_start", message_start),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], "/tmp").await;

    // UsageDelta 序列：message_start 上报 input/cache 增量，message_delta 上报 output 增量
    let deltas: Vec<(u32, u32, u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::UsageDelta {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_creation_input_tokens,
                *cache_read_input_tokens,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![(100, 0, 20, 10), (0, 30, 0, 0)],
        "每个 usage 事件的增量（相对已上报值求差分）"
    );

    // 结算总量仍与改动前一致：input 100 / output 30 / cache_creation 20 / cache_read 10
    let usage = events.iter().find_map(|e| match e {
        ChatEvent::UsageInfo {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            ..
        } => Some((
            *input_tokens,
            *output_tokens,
            *cache_creation_input_tokens,
            *cache_read_input_tokens,
        )),
        _ => None,
    });
    assert_eq!(
        usage,
        Some((100, 30, 20, 10)),
        "结算总量取流末累计值，不被 UsageDelta 重复累加"
    );
}

#[tokio::test]
async fn test_anthropic_latency_ms_recorded() {
    // 人为延迟 ≥ 20ms 后创建流，断言 latency_ms >= 20
    let message_start = r#"{"type":"message_start","message":{"id":"msg_03","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":20}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    // 先在 start_time 前 sleep，确保延迟包含在测量中
    let start = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sse = format!(
        "{}{}{}",
        sse_line("message_start", message_start),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let span = tracing::Span::current();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
        false,
        20000,
        true,
    );
    let events: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    let meta = events
        .iter()
        .find_map(|e| {
            if let ChatEvent::OutputMetadata(m) = e {
                Some(m.clone())
            } else {
                None
            }
        })
        .expect("should have OutputMetadata event");

    assert!(
        meta.latency_ms >= 20,
        "latency_ms ({}) should be >= 20",
        meta.latency_ms
    );
}

// --- 图片内容块测试 ---

/// 生成独立的临时项目目录（用于保存图片）
fn test_image_project_dir(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "visp_anthropic_image_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

#[test]
fn test_parse_image_base64() {
    let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    match result {
        ParsedEvent::ImageBlock {
            data,
            mime_type,
            remote_url,
        } => {
            assert_eq!(data.as_deref(), Some("iVBORw0KGgo="));
            assert_eq!(mime_type, "image/png");
            assert!(remote_url.is_none());
        }
        _ => panic!("expected ImageBlock, got {:?}", result),
    }
}

#[test]
fn test_parse_image_url() {
    let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"url","url":"https://example.com/image.png"}}}"#;
    let result = parse_anthropic_event("content_block_start", data).unwrap();
    match result {
        ParsedEvent::ImageBlock {
            data,
            mime_type,
            remote_url,
        } => {
            assert!(data.is_none());
            assert!(mime_type.is_empty());
            assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
        }
        _ => panic!("expected ImageBlock, got {:?}", result),
    }
}

#[tokio::test]
async fn test_byte_stream_base64_image() {
    let project_path = test_image_project_dir("base64_image");
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="}}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":1}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_stop", block_stop),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], &project_path).await;

    let image_events: Vec<&ChatEvent> = events
        .iter()
        .filter(|e| matches!(e, ChatEvent::ImageBlock { .. }))
        .collect();
    assert_eq!(image_events.len(), 1, "should have one ImageBlock event");
    if let ChatEvent::ImageBlock {
        path,
        mime_type,
        remote_url,
    } = image_events[0]
    {
        assert!(!path.is_empty(), "base64 image should be saved to disk");
        let saved = std::path::Path::new(path);
        assert_eq!(
            saved.parent().and_then(std::path::Path::file_name),
            Some(std::ffi::OsStr::new("images")),
            "image should be saved under .visp/images/, got {path}"
        );
        assert_eq!(
            saved
                .parent()
                .and_then(std::path::Path::parent)
                .and_then(std::path::Path::file_name),
            Some(std::ffi::OsStr::new(".visp")),
            "image should be saved under a project's .visp directory, got {path}"
        );
        assert_eq!(mime_type, "image/png");
        assert!(remote_url.is_none());
        // 文件确实写入磁盘
        assert!(saved.exists(), "saved image file should exist");
    } else {
        panic!("expected ImageBlock event");
    }
}

#[tokio::test]
async fn test_byte_stream_url_image() {
    let project_path = test_image_project_dir("url_image");
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"url","url":"https://example.com/image.png"}}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":1}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_stop", block_stop),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], &project_path).await;

    let image_events: Vec<&ChatEvent> = events
        .iter()
        .filter(|e| matches!(e, ChatEvent::ImageBlock { .. }))
        .collect();
    assert_eq!(image_events.len(), 1, "should have one ImageBlock event");
    if let ChatEvent::ImageBlock {
        path,
        mime_type,
        remote_url,
    } = image_events[0]
    {
        assert!(path.is_empty(), "URL image should not be saved to disk");
        assert!(mime_type.is_empty());
        assert_eq!(remote_url.as_deref(), Some("https://example.com/image.png"));
    } else {
        panic!("expected ImageBlock event");
    }
}

#[tokio::test]
async fn test_byte_stream_base64_error() {
    let project_path = test_image_project_dir("base64_error");
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start = r#"{"type":"content_block_start","index":1,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"!!!invalid-base64!!!"}}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":1}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_stop", block_stop),
        sse_line("message_stop", message_stop),
    );
    let events = collect_anthropic_events(vec![sse], &project_path).await;

    let error_events: Vec<&ChatEvent> = events
        .iter()
        .filter(|e| matches!(e, ChatEvent::ImageError { .. }))
        .collect();
    assert_eq!(error_events.len(), 1, "should have one ImageError event");
    if let ChatEvent::ImageError { reason } = error_events[0] {
        assert!(
            reason.contains("base64"),
            "error reason should mention base64, got {reason}"
        );
    } else {
        panic!("expected ImageError event");
    }
}

// --- UTF-8 跨 chunk 边界测试 ---

/// 收集 ChatEvent 流，接收字节 chunk（用于测试跨 chunk 的 UTF-8 切分）
async fn collect_anthropic_events_from_bytes(
    chunks: Vec<Vec<u8>>,
    project_path: &str,
) -> Vec<ChatEvent> {
    let byte_stream = futures::stream::iter(chunks.into_iter().map(|b| Ok(bytes::Bytes::from(b))));
    let start = std::time::Instant::now();
    let span = tracing::Span::current();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        project_path.to_string(),
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
async fn test_anthropic_utf8_multibyte_split_across_chunks() {
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"之间"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let full_bytes = sse.into_bytes();
    let zhishi = "之间".as_bytes();
    let split_offset = full_bytes
        .windows(zhishi.len())
        .position(|w| w == zhishi)
        .expect("should find 之间 in SSE");
    // 在 "之" 的第 2 字节后切分：e4 b9 | 8b e9 97 b4
    let cut = split_offset + 2;
    let chunk1 = full_bytes[..cut].to_vec();
    let chunk2 = full_bytes[cut..].to_vec();

    let events = collect_anthropic_events_from_bytes(vec![chunk1, chunk2], "/tmp").await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(
        text, "之间",
        "Anthropic Chinese text should survive chunk split"
    );
    assert!(
        !text.contains('\u{FFFD}'),
        "no U+FFFD replacement char allowed"
    );
}

#[tokio::test]
async fn test_anthropic_utf8_multibyte_split_at_char_boundary() {
    let message_start = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"test-model","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"之间"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    let sse = format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let full_bytes = sse.into_bytes();
    let zhi = "之".as_bytes();
    let split_offset = full_bytes
        .windows(zhi.len())
        .position(|w| w == zhi)
        .expect("should find 之 in SSE");
    let cut = split_offset + zhi.len();
    let chunk1 = full_bytes[..cut].to_vec();
    let chunk2 = full_bytes[cut..].to_vec();

    let events = collect_anthropic_events_from_bytes(vec![chunk1, chunk2], "/tmp").await;
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

#[test]
fn test_gen_ai_client_operation_span_created() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    // 模拟 byte_stream_to_chat_events 传 span 的处理
    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "claude-sonnet-4",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "anthropic",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
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
fn test_gen_ai_request_fields_at_span_start() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "claude-sonnet-4",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "anthropic",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    // 模拟 chat_stream 中在 span 创建后的 record
    span.record("gen_ai.system", "anthropic");
    span.record("gen_ai.request.max_tokens", 4096i64);
    span.record("gen_ai.request.temperature", 0.7f64);

    drop(_guard);
    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let fields = &spans[0].fields;
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.request.model" && v == "claude-sonnet-4")
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
            .any(|(k, v)| k == "gen_ai.system" && v == "anthropic")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.provider.name" && v == "anthropic")
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

#[test]
fn test_max_tokens_field_aligned_with_anthropic_api() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.request.max_tokens = tracing::field::Empty,
    );
    span.record("gen_ai.request.max_tokens", 8192u64);

    drop(_guard);
    let spans = spans.lock().unwrap();
    // 验证字段名为 gen_ai.request.max_tokens（不是 max_output_tokens）
    let has_field = spans[0]
        .fields
        .iter()
        .any(|(k, v)| k == "gen_ai.request.max_tokens" && v == "8192");
    assert!(has_field, "field name should be gen_ai.request.max_tokens");
}

/// 生成一组完整的 SSE 事件，用于测试 usage 字段记录
fn make_complete_sse(model: &str, stop_reason: &str) -> String {
    let message_start = format!(
        r#"{{"type":"message_start","message":{{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"{}","stop_reason":null,"stop_sequence":null,"usage":{{"input_tokens":105,"output_tokens":0,"cache_creation_input_tokens":52,"cache_read_input_tokens":200}}}}}}"#,
        model
    );
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = format!(
        r#"{{"type":"message_delta","delta":{{"stop_reason":"{}","stop_sequence":null}},"usage":{{"output_tokens":420}}}}"#,
        stop_reason
    );
    let message_stop = r#"{"type":"message_stop"}"#;

    fn sse_line(event: &str, data: &str) -> String {
        format!("event: {}\ndata: {}\n\n", event, data)
    }

    format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", &message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", &message_delta),
        sse_line("message_stop", message_stop),
    )
}

#[tokio::test]
async fn test_gen_ai_usage_fields_recorded_on_completion() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = "claude-sonnet-4-6",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "anthropic",
        gen_ai.request.max_tokens = tracing::field::Empty,
        gen_ai.request.temperature = tracing::field::Empty,
        visp.llm.attempt = 0u64,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );
    span.record("gen_ai.request.max_tokens", 4096i64);
    span.record("gen_ai.request.temperature", 0.7f64);

    let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
        false,
        20000,
        true,
    );
    let _events: Vec<ChatEvent> = event_stream
        .filter_map(|e| futures::future::ready(e.ok()))
        .collect()
        .await;

    // Wait for stream completion
    drop(_guard);

    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    let fields = &spans[0].fields;

    // 验证 usage 字段
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.input_tokens" && v == "105")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.output_tokens" && v == "420")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.cache_read.input_tokens" && v == "200")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.cache_creation.input_tokens" && v == "52")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.finish_reasons" && v == "[\"end_turn\"]")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.response.model" && v == "claude-sonnet-4-6")
    );
}

#[test]
fn test_finish_reasons_serialized_as_json_array() {
    // 验证 JSON 数组序列化
    let reasons = vec!["end_turn".to_string(), "stop".to_string()];
    let serialized = serde_json::to_string(&reasons).unwrap();
    assert_eq!(serialized, "[\"end_turn\",\"stop\"]");

    let single = vec!["end_turn".to_string()];
    assert_eq!(serde_json::to_string(&single).unwrap(), "[\"end_turn\"]");

    let empty: Vec<String> = vec![];
    assert_eq!(serde_json::to_string(&empty).unwrap(), "[]");
}

#[test]
fn test_gen_ai_client_retry_event_emitted() {
    let (_spans, events) = setup_tracing();
    let _guard = make_guard(&_spans, &events);

    // 模拟重试事件
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

#[tokio::test]
async fn test_gen_ai_client_first_token_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
    );

    let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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

#[test]
fn test_gen_ai_provider_name_is_anthropic() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let _span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.provider.name = "anthropic",
        gen_ai.operation.name = "chat",
    );

    drop(_guard);
    let spans = spans.lock().unwrap();
    assert!(
        spans[0]
            .fields
            .iter()
            .any(|(k, v)| k == "gen_ai.provider.name" && v == "anthropic")
    );
}

#[tokio::test]
async fn test_anthropic_finish_reason_max_tokens_normalized_to_length() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    // 模拟 max_tokens stop_reason
    let sse = make_complete_sse("claude-sonnet-4-6", "max_tokens");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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
        "max_tokens should be normalized to length, got: {:?}",
        fields
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "visp.llm.token_limit_hit" && v == "true"),
        "token_limit_hit should be true for max_tokens"
    );
}

#[test]
fn test_anthropic_finish_reason_end_turn_stays_unmodified() {
    let reason = crate::anthropic::normalize_anthropic_reason("end_turn");
    assert_eq!(reason, "end_turn");
}

#[test]
fn test_anthropic_finish_reason_tool_use_stays_unmodified() {
    let reason = crate::anthropic::normalize_anthropic_reason("tool_use");
    assert_eq!(reason, "tool_use");
}

#[test]
fn test_anthropic_finish_reason_unknown_stays_unmodified() {
    let reason = crate::anthropic::normalize_anthropic_reason("unknown_reason");
    assert_eq!(reason, "unknown_reason");
}

#[tokio::test]
async fn test_anthropic_token_limit_hit_when_max_tokens() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    let sse = make_complete_sse("claude-sonnet-4-6", "max_tokens");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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
    assert!(
        spans[0]
            .fields
            .iter()
            .any(|(k, v)| k == "visp.llm.token_limit_hit" && v == "true"),
        "token_limit_hit should be true for max_tokens"
    );
}

#[tokio::test]
async fn test_anthropic_token_limit_not_set_for_normal_stop() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        visp.llm.token_limit_hit = tracing::field::Empty,
    );

    let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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
    let token_limit_entry = spans[0]
        .fields
        .iter()
        .find(|(k, _)| k == "visp.llm.token_limit_hit");
    assert!(
        token_limit_entry.is_none() || token_limit_entry.unwrap().1 == "false",
        "token_limit_hit should not be true for normal stop"
    );
}

#[tokio::test]
async fn test_anthropic_cache_uses_new_dot_notation() {
    let (spans, _events) = setup_tracing();
    let _guard = make_guard(&spans, &_events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
    );

    let message_start = r#"{"type":"message_start","message":{"id":"msg_02","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200,"output_tokens":0,"cache_creation_input_tokens":80,"cache_read_input_tokens":300}}}"#;
    let block_start =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let text_delta =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
    let block_stop = r#"{"type":"content_block_stop","index":0}"#;
    let message_delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50}}"#;
    let message_stop = r#"{"type":"message_stop"}"#;

    fn sse_line(event: &str, data: &str) -> String {
        format!("event: {}\ndata: {}\n\n", event, data)
    }

    let sse = format!(
        "{}{}{}{}{}{}",
        sse_line("message_start", message_start),
        sse_line("content_block_start", block_start),
        sse_line("content_block_delta", text_delta),
        sse_line("content_block_stop", block_stop),
        sse_line("message_delta", message_delta),
        sse_line("message_stop", message_stop),
    );
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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
            .any(|(k, v)| k == "gen_ai.usage.cache_read.input_tokens" && v == "300")
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "gen_ai.usage.cache_creation.input_tokens" && v == "80")
    );
    // 确认旧的下划线字段名不存在
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
}

#[tokio::test]
async fn test_gen_ai_client_completed_event() {
    let (spans, events) = setup_tracing();
    let _guard = make_guard(&spans, &events);

    let span = tracing::info_span!(
        "gen_ai.client.operation",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.response.model = tracing::field::Empty,
    );

    let sse = make_complete_sse("claude-sonnet-4-6", "end_turn");
    let byte_stream =
        futures::stream::iter(vec![sse].into_iter().map(|s| Ok(bytes::Bytes::from(s))));
    let start = std::time::Instant::now();
    let event_stream = byte_stream_to_chat_events(
        byte_stream,
        start,
        span,
        "test-model".to_string(),
        "/tmp".to_string(),
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

// --- build_anthropic_request thinking 合规测试 ---

fn make_anthropic_config(temp: f64, max_tokens: u32, thinking_budget: Option<&str>) -> LlmConfig {
    let mut config = LlmConfig {
        model: "glm-5.2".into(),
        temperature: temp,
        max_tokens,
        ..Default::default()
    };
    if let Some(b) = thinking_budget {
        config
            .extra
            .insert("thinking_budget_tokens".into(), b.into());
    }
    config
}

#[test]
fn test_anthropic_thinking_forces_temperature_one() {
    let config = make_anthropic_config(0.7, 8000, Some("12800"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["temperature"].as_f64(), Some(1.0));
}

#[test]
fn test_anthropic_thinking_budget_clamped_when_exceeds_max_tokens() {
    let config = make_anthropic_config(0.7, 8000, Some("12800"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(7999));
}

#[test]
fn test_anthropic_thinking_budget_clamped_when_equals_max_tokens() {
    let config = make_anthropic_config(0.7, 8000, Some("8000"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(7999));
}

#[test]
fn test_anthropic_thinking_budget_kept_when_below_max_tokens() {
    let config = make_anthropic_config(0.7, 8000, Some("4096"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["thinking"]["budget_tokens"].as_u64(), Some(4096));
}

#[test]
fn test_anthropic_no_thinking_keeps_temperature() {
    let config = make_anthropic_config(0.7, 8000, None);
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["temperature"].as_f64(), Some(0.7));
    assert!(req.get("thinking").is_none());
}

#[test]
fn test_anthropic_thinking_invalid_budget_skipped() {
    let config = make_anthropic_config(0.7, 8000, Some("not-a-number"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert!(req.get("thinking").is_none());
    assert_eq!(req["temperature"].as_f64(), Some(0.7));
}

#[test]
fn test_anthropic_thinking_skipped_when_max_tokens_zero() {
    let config = make_anthropic_config(0.7, 0, Some("12800"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert!(req.get("thinking").is_none());
    assert_eq!(req["temperature"].as_f64(), Some(0.7));
}

#[test]
fn test_anthropic_thinking_temperature_idempotent() {
    let config = make_anthropic_config(1.0, 8000, Some("4096"));
    let req = build_anthropic_request(&[Message::user("Hi")], &[], &config);
    assert_eq!(req["temperature"].as_f64(), Some(1.0));
}
