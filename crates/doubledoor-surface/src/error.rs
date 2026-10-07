//! Typed errors for surface indexing and delta computation.

use thiserror::Error;

use doubledoor_core::IdError;
use doubledoor_git::GitError;

/// Failure modes of the surface layer.
#[derive(Debug, Error)]
pub enum SurfaceError {
    /// A `git` invocation failed (transparent passthrough of the choke-point
    /// error, argv/code/stderr included).
    #[error(transparent)]
    Git(#[from] GitError),

    /// A repo-relative path violated §27 path hygiene (absolute, traversing,
    /// or empty).
    #[error("invalid repo-relative path: {0}")]
    InvalidPath(String),

    /// A surface identifier failed validation.
    #[error(transparent)]
    Id(#[from] IdError),

    /// A `git diff-tree --name-status` line did not match the expected
    /// `<status>\t<path>[ \t<path>]` shape.
    #[error("malformed name-status line: {0}")]
    MalformedLine(String),

    /// A `git diff-tree --name-status` line carried a status letter this
    /// layer does not understand. Surface arithmetic must never silently
    /// drop a change, so unrecognized output is a hard error.
    #[error("unsupported name-status letter: {0}")]
    UnknownStatus(String),
}
