use std::path::PathBuf;

/// Top-level framework error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("budget exceeded: {reason}")]
    BudgetExceeded { reason: String },

    #[error("boundary violation: {paths:?} touch protected paths")]
    BoundaryViolation { paths: Vec<PathBuf> },

    #[error("tool violation: command `{command}` blocked by policy")]
    ToolViolation { command: String, reason: String },

    #[error("agent `{agent}` failed: {reason}")]
    Agent { agent: String, reason: String },

    #[error("pipeline stage `{stage}` failed: {reason}")]
    Pipeline { stage: String, reason: String },

    #[error("failed to parse agent output: {reason}")]
    Parse { reason: String, raw: String },

    #[error("config error: {0}")]
    Config(String),

    #[error("workspace error: {0}")]
    Workspace(String),

    #[error("governance denied action: {0}")]
    GovernanceDenied(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
