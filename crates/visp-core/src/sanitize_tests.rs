use super::*;

// ── truncate tests ───────────────────────────────────────────────────────

#[test]
fn test_truncate_short_string() {
    assert_eq!(truncate("hello", 10), "hello");
}

#[test]
fn test_truncate_exact() {
    assert_eq!(truncate("hello", 5), "hello");
}

#[test]
fn test_truncate_adds_marker() {
    let result = truncate("hello world", 5);
    assert_eq!(result, "hello…[truncated]");
}

#[test]
fn test_truncate_utf8_safe() {
    let s = "你好世界abcd";
    let result = truncate(s, 4);
    // First 4 chars: "你好世界" → truncated, so result is "你好世界…[truncated]"
    assert_eq!(result, "你好世界…[truncated]");
}

#[test]
fn test_truncate_utf8_boundary() {
    let s = "你好世";
    // 3 chars, limit 4 → no truncation
    let result = truncate(s, 4);
    assert_eq!(result, "你好世");
}

// ── sanitize_json tests ─────────────────────────────────────────────────

#[test]
fn test_redact_api_key_field() {
    let input = r#"{"api_key": "sk-abc123xyz", "model": "gpt-4"}"#;
    let result = sanitize_json(input);
    assert!(result.contains(r#""[REDACTED]"#));
    assert!(result.contains("gpt-4"));
    assert!(!result.contains("sk-abc123xyz"));
}

#[test]
fn test_redact_authorization_field() {
    let input = r#"{"authorization": "Bearer token12345", "model": "claude"}"#;
    let result = sanitize_json(input);
    assert!(result.contains(r#""[REDACTED]"#));
    assert!(!result.contains("Bearer"));
}

#[test]
fn test_redact_nested_sensitive_field() {
    let input = r#"{"config": {"api_key": "sk-secret", "name": "test"}, "model": "claude"}"#;
    let result = sanitize_json(input);
    assert!(result.contains(r#""[REDACTED]"#));
    assert!(result.contains("test"));
    assert!(!result.contains("sk-secret"));
}

#[test]
fn test_redact_array_nested() {
    let input = r#"{"models": [{"name": "gpt-4", "api_key": "sk-xxx"}]}"#;
    let result = sanitize_json(input);
    assert!(result.contains(r#""[REDACTED]"#));
    assert!(result.contains("gpt-4"));
    assert!(!result.contains("sk-xxx"));
}

#[test]
fn test_redact_bearer_in_string() {
    let input = r#"some text with Bearer sk-abc123xyz and stuff"#;
    let result = simple_redact(input);
    assert!(!result.contains("sk-abc123xyz"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn test_redact_non_json_string() {
    let input = "Authorization: Bearer tok_abcdef123456\napi_key=sk-secret-key";
    let result = sanitize_json(input);
    // Falls back to simple_redact for non-JSON
    assert!(result.contains("[REDACTED]"));
    assert!(!result.contains("tok_abcdef123456"));
}

#[test]
fn test_sanitize_sk_prefix() {
    let input = r#"{"key": "sk-test-key-value"}"#;
    let result = sanitize_json(input);
    // "key" is not in sensitive keys list, but redact_string_values applies
    // simple_redact to all string values, catching sk- patterns
    assert!(
        !result.contains("sk-test-key-value"),
        "sk- value should be redacted"
    );
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn test_sanitize_and_truncate_combined() {
    let input = r#"{"api_key": "sk-very-secret", "output": "hello world this is a long text"}"#;
    let result = sanitize_and_truncate(input, 30, true);
    assert!(result.contains("[REDACTED]"));
    assert!(result.contains("[truncated]") || result.len() < input.len());
}

#[test]
fn test_no_redact_when_disabled() {
    let input = r#"{"api_key": "sk-secret", "model": "gpt-4"}"#;
    let result = sanitize_and_truncate(input, 100, false);
    assert!(result.contains("sk-secret"));
}

#[test]
fn test_format_langfuse_input_with_system_prompt() {
    let body = serde_json::json!({
        "model": "claude-sonnet-4",
        "system": [{"type": "text", "text": "You are a helpful assistant."}],
        "messages": [{"role": "user", "content": "Hi"}],
        "max_tokens": 4096,
    });
    let result = format_langfuse_input(&body, 10000, true);
    assert!(result.contains("You are a helpful assistant."));
    assert!(result.contains("claude-sonnet-4"));
}

#[test]
fn test_format_langfuse_output_simple() {
    let result = format_langfuse_output("Hello, how can I help?", 10000, true);
    assert_eq!(result, "Hello, how can I help?");
}
