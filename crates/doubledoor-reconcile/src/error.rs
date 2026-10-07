//! Typed reconciliation errors.

use thiserror::Error;

use doubledoor_core::GitOid;
use doubledoor_git::GitError;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("reconcile I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("candidate record is corrupt: {0}")]
    Corrupt(String),
    /// The candidate is not derived from the state it claims (§23).
    #[error("unknown or inconsistent state: {0}")]
    UnknownState(String),
    /// DD-I4: canonical moved past the candidate's target.
    #[error("canonical HEAD advanced: candidate targets {expected}, canonical is {actual}")]
    CanonicalStateAdvanced { expected: GitOid, actual: GitOid },
    /// The canonical worktree refused the fast-forward (e.g. uncommitted
    /// local changes would be overwritten). Canonical is unchanged.
    #[error("canonical worktree refused the promotion (nothing changed): {0}")]
    CanonicalRefused(String),
    /// Selective import could not be applied without overwriting the
    /// session's own changes.
    #[error("cannot import: {0}")]
    ImportConflict(String),
    #[error("{0}")]
    Usage(String),
}
