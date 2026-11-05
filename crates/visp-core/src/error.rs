use thiserror::Error;

/// 顶层核心错误，所有子系统错误向上传播到这里
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

/// LLM 服务相关错误
#[derive(Error, Debug)]
pub enum LlmError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Rate limited, retry after {retry_after_secs}s")]
    RateLimit { retry_after_secs: u64 },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("LLM call cancelled")]
    Cancelled,
}

/// 会话相关错误
#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),

    #[error("Session already exists: {0}")]
    AlreadyExists(String),

    #[error("Session is not running: {0}")]
    NotRunning(String),

    #[error("Session busy: {session_id}")]
    SessionBusy { session_id: String },

    #[error("Protocol error: {message}")]
    ProtocolError { message: String },

    #[error("{0}")]
    Other(String),
}

/// Agent 内部错误码，用于 AgentEvent::Error
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentErrorCode {
    LlmAuth,
    LlmRateLimit,
    LlmApi,
    LlmNetwork,
    LlmStream,
    MaxIterations,
    StuckInLoop,
    Cancelled,
    Internal,
    SessionBusy,
}

impl std::fmt::Display for AgentErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentErrorCode::LlmAuth => write!(f, "LLM authentication error"),
            AgentErrorCode::LlmRateLimit => write!(f, "LLM rate limit exceeded"),
            AgentErrorCode::LlmApi => write!(f, "LLM API error"),
            AgentErrorCode::LlmNetwork => write!(f, "LLM network error"),
            AgentErrorCode::LlmStream => write!(f, "LLM stream error"),
            AgentErrorCode::MaxIterations => write!(f, "Maximum iterations reached"),
            AgentErrorCode::StuckInLoop => write!(f, "Agent stuck in repeated tool call loop"),
            AgentErrorCode::Cancelled => write!(f, "Operation cancelled"),
            AgentErrorCode::Internal => write!(f, "Internal error"),
            AgentErrorCode::SessionBusy => write!(f, "SessionBusy"),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
