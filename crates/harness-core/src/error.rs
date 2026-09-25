use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid configuration: {reason}")]
    InvalidConfig { reason: String },
    #[error("invalid identifier: {reason}")]
    InvalidId { reason: String },
    #[error("invalid request: {reason}")]
    InvalidRequest { reason: String },
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: crate::Id },
    #[error("permission denied for {capability}")]
    PermissionDenied { capability: String },
    #[error("model provider {provider} failed: {message}")]
    Provider { provider: String, message: String },
    #[error("tool {tool} failed: {message}")]
    Tool { tool: String, message: String },
    #[error("verification failed: {message}")]
    Verification { message: String },
    #[error("logging initialization failed: {0}")]
    Logging(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
