use super::*;
use std::collections::HashMap;

#[test]
fn test_llmconfig_default() {
    let config = LlmConfig::default();
    assert_eq!(config.model, "claude-3-7-sonnet-20250219");
    assert_eq!(config.temperature, 0.7);
    assert_eq!(config.max_tokens, 4096);
    assert!(config.extra.is_empty());
}

#[test]
fn test_llmconfig_capture_defaults() {
    let config = LlmConfig::default();
    assert!(
        !config.langfuse_capture_input,
        "capture_input should default to false"
    );
    assert!(
        !config.langfuse_capture_output,
        "capture_output should default to false"
    );
    assert_eq!(config.langfuse_capture_max_chars, 20000);
    assert!(config.langfuse_redact_secrets);
}

#[test]
fn test_llmconfig_capture_configured() {
    let config = LlmConfig {
        langfuse_capture_input: true,
        langfuse_capture_output: false,
        langfuse_capture_max_chars: 5000,
        langfuse_redact_secrets: false,
        ..Default::default()
    };
    assert!(config.langfuse_capture_input);
    assert!(!config.langfuse_capture_output);
    assert_eq!(config.langfuse_capture_max_chars, 5000);
    assert!(!config.langfuse_redact_secrets);
}

#[test]
fn test_llmconfig_extra() {
    let mut config = LlmConfig::default();
    config.extra.insert("key".to_string(), "value".to_string());
    assert_eq!(config.extra.get("key").unwrap(), "value");
}

#[test]
fn test_llmconfig_default_max_context_tokens() {
    let config = LlmConfig::default();
    assert_eq!(config.max_context_tokens, 128_000);
}

#[test]
fn test_llmconfig_custom_max_context_tokens() {
    let config = LlmConfig {
        max_context_tokens: 64_000,
        ..Default::default()
    };
    assert_eq!(config.max_context_tokens, 64_000);
}

// ── Langfuse trace 级字段测试 ──────────────────────────────────────────

#[test]
fn test_llmconfig_langfuse_trace_defaults() {
    let config = LlmConfig::default();
    assert!(
        !config.langfuse_enabled,
        "langfuse_enabled should default to false"
    );
    assert_eq!(config.langfuse_session_id, None);
    assert_eq!(config.langfuse_trace_name, None);
    assert_eq!(config.langfuse_user_id, None);
    assert_eq!(config.langfuse_tags, None);
    assert_eq!(config.langfuse_environment, None);
    assert_eq!(config.langfuse_release, None);
    assert_eq!(config.langfuse_version, None);
    assert_eq!(config.langfuse_public, None);
    assert_eq!(config.langfuse_metadata, None);
}

#[test]
fn test_llmconfig_langfuse_trace_configured() {
    let mut meta = HashMap::new();
    meta.insert("env".into(), "prod".into());
    let config = LlmConfig {
        langfuse_enabled: true,
        langfuse_session_id: Some("sess_abc".into()),
        langfuse_trace_name: Some("visp.agent.run".into()),
        langfuse_user_id: Some("user_789".into()),
        langfuse_tags: Some(r#"["agent"]"#.into()),
        langfuse_environment: Some("staging".into()),
        langfuse_release: Some("1.0.0".into()),
        langfuse_version: Some("abc123".into()),
        langfuse_public: Some(true),
        langfuse_metadata: Some(meta.clone()),
        ..Default::default()
    };
    assert!(config.langfuse_enabled);
    assert_eq!(config.langfuse_session_id.as_deref(), Some("sess_abc"));
    assert_eq!(
        config.langfuse_trace_name.as_deref(),
        Some("visp.agent.run")
    );
    assert_eq!(config.langfuse_user_id.as_deref(), Some("user_789"));
    assert_eq!(config.langfuse_tags.as_deref(), Some(r#"["agent"]"#));
    assert_eq!(config.langfuse_environment.as_deref(), Some("staging"));
    assert_eq!(config.langfuse_release.as_deref(), Some("1.0.0"));
    assert_eq!(config.langfuse_version.as_deref(), Some("abc123"));
    assert_eq!(config.langfuse_public, Some(true));
    assert_eq!(config.langfuse_metadata, Some(meta));
}

#[test]
fn test_llmconfig_langfuse_partial_trace() {
    let config = LlmConfig {
        langfuse_enabled: true,
        langfuse_user_id: Some("partial".into()),
        langfuse_environment: Some("default".into()),
        ..Default::default()
    };
    assert!(config.langfuse_enabled);
    assert_eq!(config.langfuse_user_id.as_deref(), Some("partial"));
    assert_eq!(config.langfuse_environment.as_deref(), Some("default"));
    // Other fields should remain None
    assert_eq!(config.langfuse_session_id, None);
    assert_eq!(config.langfuse_trace_name, None);
    assert_eq!(config.langfuse_tags, None);
    assert_eq!(config.langfuse_release, None);
    assert_eq!(config.langfuse_version, None);
    assert_eq!(config.langfuse_public, None);
    assert_eq!(config.langfuse_metadata, None);
}

#[test]
fn test_llmconfig_langfuse_capture_defaults_unchanged() {
    // Verify existing capture defaults are not affected by new trace fields
    let config = LlmConfig::default();
    assert!(!config.langfuse_capture_input);
    assert!(!config.langfuse_capture_output);
    assert_eq!(config.langfuse_capture_max_chars, 20000);
    assert!(config.langfuse_redact_secrets);
}
