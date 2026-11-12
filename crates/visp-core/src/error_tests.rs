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

#[test]
fn test_context_length_error_detected_for_common_provider_messages() {
    let messages = [
        (
            400,
            "This model's maximum context length is 32768 tokens. However, you requested 33000 tokens.",
        ),
        (
            400,
            "context_length_exceeded: reduce the length of the messages",
        ),
        (400, "prompt is too long: 21000 tokens > 20000 maximum"),
        (413, "request entity too large"),
        (400, "输入过长：上下文长度超出模型上限"),
    ];
    for (status, message) in messages {
        let err = LlmError::Api {
            status,
            message: message.to_string(),
        };
        assert!(
            err.is_context_length_exceeded(),
            "expected context-length detection for {message:?}"
        );
    }
}

#[test]
fn test_context_length_error_not_falsely_detected() {
    let messages = [
        (401, "invalid api key"),
        (400, "tool_calls must be followed by tool messages"),
        (429, "rate limit exceeded, too many tokens per minute"),
        (500, "maximum context length exceeded"),
    ];
    for (status, message) in messages {
        let err = LlmError::Api {
            status,
            message: message.to_string(),
        };
        assert!(
            !err.is_context_length_exceeded(),
            "unexpected context-length detection for {message:?}"
        );
    }
    assert!(!LlmError::Network("context length".into()).is_context_length_exceeded());
    assert!(!LlmError::Cancelled.is_context_length_exceeded());
}
