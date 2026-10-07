//! Typed policy errors (§41: libraries avoid opaque stringly-typed errors).

use thiserror::Error;

/// Why a session-open request was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The active-session quota is already saturated.
    MaxActiveSessions { current: usize, max: u32 },
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenyReason::MaxActiveSessions { current, max } => {
                write!(f, "active session count {current} already at quota {max}")
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    #[error("request denied: {reason}")]
    Denied { reason: DenyReason },
}
