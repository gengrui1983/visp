use super::*;

#[test]
fn test_provider_metadata_has_required_fields() {
    let meta = ProviderMetadata {
        model: "claude-sonnet-4-20250514".into(),
        finish_reasons: vec!["end_turn".into()],
        input_tokens: 105,
        output_tokens: 420,
        cache_read_input_tokens: Some(200),
        cache_creation_input_tokens: Some(52),
        latency_ms: 1234,
    };
    assert_eq!(meta.model, "claude-sonnet-4-20250514");
    assert_eq!(meta.finish_reasons, vec!["end_turn"]);
    assert_eq!(meta.input_tokens, 105);
    assert_eq!(meta.output_tokens, 420);
    assert_eq!(meta.cache_read_input_tokens, Some(200));
    assert_eq!(meta.cache_creation_input_tokens, Some(52));
    assert_eq!(meta.latency_ms, 1234);
}

#[test]
fn test_provider_metadata_has_required_fields_cache_none() {
    // 当 cache 字段为 None 时（如 OpenAI）
    let meta = ProviderMetadata {
        model: "gpt-4o".into(),
        finish_reasons: vec!["stop".into()],
        input_tokens: 50,
        output_tokens: 100,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        latency_ms: 567,
    };
    assert_eq!(meta.model, "gpt-4o");
    assert_eq!(meta.finish_reasons, vec!["stop"]);
    assert_eq!(meta.input_tokens, 50);
    assert_eq!(meta.output_tokens, 100);
    assert_eq!(meta.cache_read_input_tokens, None);
    assert_eq!(meta.cache_creation_input_tokens, None);
    assert_eq!(meta.latency_ms, 567);
}

#[test]
fn test_provider_metadata_serializes_to_json() {
    let meta = ProviderMetadata {
        model: "claude-sonnet-4-20250514".into(),
        finish_reasons: vec!["end_turn".into()],
        input_tokens: 105,
        output_tokens: 420,
        cache_read_input_tokens: Some(200),
        cache_creation_input_tokens: Some(52),
        latency_ms: 1234,
    };
    let json = serde_json::to_string(&meta).expect("serialize ProviderMetadata");
    // 验证包含所有必需字段
    assert!(json.contains(r#""model":"claude-sonnet-4-20250514""#));
    assert!(json.contains(r#""finish_reasons":["end_turn"]"#));
    assert!(json.contains(r#""input_tokens":105"#));
    assert!(json.contains(r#""output_tokens":420"#));
    assert!(json.contains(r#""cache_read_input_tokens":200"#));
    assert!(json.contains(r#""cache_creation_input_tokens":52"#));
    assert!(json.contains(r#""latency_ms":1234"#));
}

#[test]
fn test_provider_metadata_deserializes_from_json() {
    let json = r#"{
        "model": "gpt-4o",
        "finish_reasons": ["stop"],
        "input_tokens": 50,
        "output_tokens": 100,
        "cache_read_input_tokens": null,
        "cache_creation_input_tokens": null,
        "latency_ms": 567
    }"#;
    let meta: ProviderMetadata = serde_json::from_str(json).expect("deserialize ProviderMetadata");
    assert_eq!(meta.model, "gpt-4o");
    assert_eq!(meta.finish_reasons, vec!["stop"]);
    assert_eq!(meta.input_tokens, 50);
    assert_eq!(meta.output_tokens, 100);
    assert_eq!(meta.cache_read_input_tokens, None);
    assert_eq!(meta.cache_creation_input_tokens, None);
    assert_eq!(meta.latency_ms, 567);
}

#[test]
fn test_provider_metadata_json_roundtrip() {
    let meta = ProviderMetadata {
        model: "claude-sonnet-4-20250514".into(),
        finish_reasons: vec!["end_turn".into()],
        input_tokens: 105,
        output_tokens: 420,
        cache_read_input_tokens: Some(200),
        cache_creation_input_tokens: Some(52),
        latency_ms: 1234,
    };
    let json = serde_json::to_string(&meta).unwrap();
    let deserialized: ProviderMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, meta);
}
