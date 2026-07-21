use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("model error: {message}")]
pub struct ModelError {
    pub message: String,
    pub retryable: bool,
}

impl ModelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("tool error: {message}")]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("policy rejected tool call: {message}")]
pub struct PolicyError {
    pub message: String,
}

impl PolicyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error("model returned a message with role {0:?}; expected assistant")]
    InvalidModelResponse(crate::Role),
    #[error("duplicate tool name: {0}")]
    DuplicateTool(String),
    #[error("maximum number of turns exceeded ({0})")]
    MaxTurnsExceeded(usize),
    #[error("maximum number of tool calls exceeded ({0})")]
    MaxToolCallsExceeded(usize),
    #[error("run timed out")]
    Timeout,
    #[error("run cancelled")]
    Cancelled,
}
