//! Typed journal errors (§41: libraries avoid opaque stringly-typed errors).

use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error("cbor error: {0}")]
    Cbor(String),
    #[error("corrupt journal line {line}: {reason}")]
    Corrupt { line: usize, reason: String },
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    #[error("session path is not a directory: {0}")]
    BadSessionDir(PathBuf),
}
