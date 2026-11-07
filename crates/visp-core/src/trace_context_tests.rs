use super::*;

// ── W1-S1-1: basic construction & validation ──────────────────────────

#[test]
fn test_trace_context_w3c_format() {
    let trace_id = "0af7651916cd43dd8448eb211c80319c".to_string();
    let span_id = "b7ad6b7169203331".to_string();
    let tc = TraceContext::new(trace_id.clone(), span_id.clone(), 1, None, None).unwrap();
    assert_eq!(tc.trace_id, trace_id);
    assert_eq!(tc.span_id, span_id);
    assert_eq!(tc.trace_flags, 1);
    assert_eq!(tc.trace_state, None);
    assert_eq!(tc.parent_span_id, None);
    assert_eq!(tc.trace_id.len(), 32);
    assert_eq!(tc.span_id.len(), 16);
}

#[test]
fn test_trace_context_invalid_length_rejected() {
    // trace_id too short (30 hex chars)
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c8031".to_string(), // 30 chars
        "b7ad6b7169203331".to_string(),
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceIdLength);

    // trace_id too long (34 hex chars)
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319cde".to_string(), // 34 chars
        "b7ad6b7169203331".to_string(),
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceIdLength);

    // span_id too short (14 hex chars)
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b71692033".to_string(), // 14 chars
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidSpanIdLength);

    // span_id too long (18 hex chars)
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331de".to_string(), // 18 chars
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidSpanIdLength);
}

#[test]
fn test_trace_context_invalid_hex_rejected() {
    // Non-hex character in trace_id (z is not hex)
    let err = TraceContext::new(
        "zaf7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidHex);

    // Non-hex character in span_id (g is not hex)
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "g7ad6b7169203331".to_string(),
        0,
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidHex);
}

// ── Clone + PartialEq + Debug ─────────────────────────────────────────

#[test]
fn test_trace_context_clone_eq() {
    let tc = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        Some("congo=toto".to_string()),
        None,
    )
    .unwrap();
    let cloned = tc.clone();
    assert_eq!(tc, cloned);
    // Debug output contains key fields
    let debug_str = format!("{tc:?}");
    assert!(debug_str.contains("0af7651916cd43dd8448eb211c80319c"));
}

// ── Serde roundtrip ───────────────────────────────────────────────────

#[test]
fn test_trace_context_serde_roundtrip() {
    let tc = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        None,
        None,
    )
    .unwrap();
    let json = serde_json::to_string(&tc).unwrap();
    let deser: TraceContext = serde_json::from_str(&json).unwrap();
    assert_eq!(tc, deser);

    // With trace_state
    let tc2 = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        Some("congo=toto".to_string()),
        None,
    )
    .unwrap();
    let json2 = serde_json::to_string(&tc2).unwrap();
    let deser2: TraceContext = serde_json::from_str(&json2).unwrap();
    assert_eq!(tc2, deser2);
}

// ── W3C traceparent header parsing ────────────────────────────────────

#[test]
fn test_trace_context_from_w3c_traceparent_header() {
    // Valid traceparent
    let s = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let tc = TraceContext::from_traceparent(s).unwrap();
    assert_eq!(tc.trace_id, "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(tc.span_id, "b7ad6b7169203331");
    assert_eq!(tc.trace_flags, 1);
    assert_eq!(tc.trace_state, None);

    // Valid traceparent with flags=00
    let s2 = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00";
    let tc2 = TraceContext::from_traceparent(s2).unwrap();
    assert_eq!(tc2.trace_flags, 0);
}

#[test]
fn test_trace_context_from_traceparent_invalid() {
    // Wrong number of fields (only 3 parts)
    let err =
        TraceContext::from_traceparent("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331")
            .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceparent);

    // Wrong number of fields (5 parts)
    let err = TraceContext::from_traceparent(
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceparent);

    // Version not 00
    let err =
        TraceContext::from_traceparent("01-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
            .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceparent);

    // Bad hex in trace_id (32 chars, invalid hex digit 'x')
    let err =
        TraceContext::from_traceparent("00-xxxx1916cd43dd8448eb211c80319cAB-b7ad6b7169203331-01")
            .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidHex);

    // Empty string
    let err = TraceContext::from_traceparent("").unwrap_err();
    assert_eq!(err, TraceContextError::InvalidTraceparent);
}

// ── P0-1: parent_span_id ────────────────────────────────────────────────

#[test]
fn test_trace_context_with_parent_span_id() {
    let tc = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        1,
        None,
        Some("aaaaaaaaaaaaaaaa".to_string()),
    )
    .unwrap();
    assert_eq!(tc.parent_span_id.as_deref(), Some("aaaaaaaaaaaaaaaa"));
    // Serialize/deserialize roundtrip preserves parent_span_id
    let json = serde_json::to_string(&tc).unwrap();
    let deser: TraceContext = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.parent_span_id.as_deref(), Some("aaaaaaaaaaaaaaaa"));
}

#[test]
fn test_trace_context_parent_span_id_hex_validation() {
    // Non-hex parent_span_id is rejected
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        0,
        None,
        Some("zzzzzzzzzzzzzzzz".to_string()),
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidHex);

    // Wrong length parent_span_id
    let err = TraceContext::new(
        "0af7651916cd43dd8448eb211c80319c".to_string(),
        "b7ad6b7169203331".to_string(),
        0,
        None,
        Some("1234".to_string()),
    )
    .unwrap_err();
    assert_eq!(err, TraceContextError::InvalidSpanIdLength);
}

#[test]
fn test_trace_context_traceparent_parsing_leaves_parent_none() {
    // When parsing from a W3C traceparent, parent_span_id is always None
    let s = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let tc = TraceContext::from_traceparent(s).unwrap();
    assert_eq!(tc.parent_span_id, None);
}

// ── P0-2: generate_w3c_span_id + SpanW3CId ─────────────────────────────

#[test]
fn test_generate_w3c_span_id_length_and_hex() {
    let id = generate_w3c_span_id();
    assert_eq!(id.len(), 16, "W3C span ID must be exactly 16 hex chars");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit()),
        "all characters must be hex"
    );
    // Ensure lowercase
    assert_eq!(id, id.to_ascii_lowercase());
}

#[test]
fn test_generate_w3c_span_id_unique() {
    let id1 = generate_w3c_span_id();
    let id2 = generate_w3c_span_id();
    assert_ne!(id1, id2, "successive calls must produce different IDs");
}
