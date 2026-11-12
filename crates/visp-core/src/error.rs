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

impl LlmError {
    /// 是否为「输入超过模型上下文窗口」类错误。
    ///
    /// 不同 provider 的文案差异很大（OpenAI/vLLM、Anthropic、各类网关、
    /// 中文错误信息），因此基于状态码 + 关键词组合判断，而不是模型名称。
    /// 仅 4xx（400/413/422）参与判断，避免把 5xx 或限流误判为上下文超长。
    pub fn is_context_length_exceeded(&self) -> bool {
        let LlmError::Api { status, message } = self else {
            return false;
        };
        if !matches!(*status, 400 | 413 | 422) {
            return false;
        }

        let message = message.to_lowercase();
        const PHRASES: &[&str] = &[
            "context length",
            "context_length",
            "context window",
            "maximum context",
            "max context",
            "prompt is too long",
            "input is too long",
            "too many tokens",
            "reduce the length",
            "maximum number of tokens",
            "exceeds the model",
            "request entity too large",
            "上下文长度",
            "上下文超",
            "超出最大",
            "输入过长",
        ];
        if PHRASES.iter().any(|p| message.contains(p)) {
            return true;
        }

        // 兜底：同时出现 token 与“超限”语义（不同网关措辞不统一）。
        message.contains("token") && (message.contains("exceed") || message.contains("too long"))
    }
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
