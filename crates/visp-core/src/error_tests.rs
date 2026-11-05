use super::*;

#[test]
fn test_session_busy_display() {
    let err = SessionError::SessionBusy {
        session_id: "x".into(),
    };
    assert!(err.to_string().contains("x"));
}

#[test]
fn test_session_protocol_error_display() {
    let err = SessionError::ProtocolError {
        message: "mismatch".into(),
    };
    assert!(err.to_string().contains("mismatch"));
}

#[test]
fn test_agent_error_code_display() {
    let cases: Vec<(AgentErrorCode, &str)> = vec![
        (AgentErrorCode::LlmAuth, "LLM authentication error"),
        (AgentErrorCode::LlmRateLimit, "LLM rate limit exceeded"),
        (AgentErrorCode::LlmApi, "LLM API error"),
        (AgentErrorCode::LlmNetwork, "LLM network error"),
        (AgentErrorCode::LlmStream, "LLM stream error"),
        (AgentErrorCode::MaxIterations, "Maximum iterations reached"),
        (AgentErrorCode::Cancelled, "Operation cancelled"),
        (AgentErrorCode::Internal, "Internal error"),
        (
            AgentErrorCode::StuckInLoop,
            "Agent stuck in repeated tool call loop",
        ),
        (AgentErrorCode::SessionBusy, "SessionBusy"),
    ];
    for (code, expected) in cases {
        assert_eq!(code.to_string(), expected);
    }
}
