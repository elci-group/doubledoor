//! Typed errors for Git operations. No stringly errors beyond the structured
//! [`GitError::Command`] payload, which carries the exact argv, exit status,
//! and stderr for diagnosis.

use std::path::PathBuf;

use thiserror::Error;

/// Failure modes of the Git integration layer.
#[derive(Debug, Error)]
pub enum GitError {
    /// `git rev-parse --show-toplevel` could not locate a repository at or
    /// above the requested starting directory (or the `git` binary itself is
    /// unavailable).
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),

    /// A `git` invocation failed. `code` is `None` when the process could not
    /// be spawned at all (e.g. `git` not on `PATH`); `stderr` then carries the
    /// OS error text.
    #[error("git {args:?} failed (exit code {code:?}): {stderr}")]
    Command {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },

    /// Git succeeded but produced output this layer cannot interpret
    /// (e.g. stdout that is not a valid object id).
    #[error("unexpected git output: {0}")]
    UnexpectedOutput(String),

    /// A computed on-disk path would escape the repository root (§27, path
    /// hygiene). The payload is the offending path.
    #[error("path escapes the repository root: {0}")]
    PathEscape(PathBuf),
}
