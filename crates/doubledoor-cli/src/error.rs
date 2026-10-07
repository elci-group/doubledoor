//! Typed CLI errors, each carrying its stable process exit code (§30).
//!
//! The mapping between failure modes and exit codes is part of the public
//! contract: scripts key off these codes, so they are documented in
//! `doubledoor --help` and never reused for other meanings.

use thiserror::Error;

use doubledoor_ghost::GhostError;
use doubledoor_git::GitError;
use doubledoor_journal::JournalError;
use doubledoor_policy::PolicyError;
use doubledoor_surface::SurfaceError;

/// Every failure the CLI can report, tagged with its stable exit code.
#[derive(Debug, Error)]
pub enum CliError {
    /// Exit 1: bad invocation, unknown flags, or no repository resolvable.
    #[error("{0}")]
    Usage(String),

    /// Exit 2: the live policy denied the session-open request.
    #[error("policy denied: {0}")]
    PolicyDenied(String),

    /// Exit 3: the request contends but no structurally valid contention
    /// notice was supplied (DD-I6: Doubledoor never fabricates authority).
    #[error("contention without notice: {0}")]
    ContentionWithoutNotice(String),

    /// Exit 4: workspace isolation failed; the session is FAILED_ISOLATION.
    #[error("isolation failed: {0}")]
    IsolationFailed(String),

    /// Exit 5: the requested session does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Exit 6: the requested state transition is not legal from the session's
    /// current state.
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    /// Exit 7: on-disk session metadata or journal is corrupt or written in
    /// an unsupported schema.
    #[error("corrupt metadata: {0}")]
    CorruptMetadata(String),

    /// Exit 8: an unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),

    /// Exit 9: the subcommand is declared for a later phase and intentionally
    /// not implemented. Reserved: every §30 command is implemented as of
    /// Phase IV, but the code keeps its meaning for future declarations.
    #[allow(dead_code)]
    #[error("not implemented in this phase: {0}")]
    NotImplementedPhase(String),

    /// Exit 10: an accepted ghost cannot enter the workspace cleanly
    /// (uncommitted edits to the imported paths, or the delta does not
    /// apply). The workspace was left unchanged.
    #[error("ghost conflict: {0}")]
    GhostConflict(String),

    /// Exit 11: the Locksmith lease does not permit the canonical mutation
    /// (missing, unreadable, held by another actor, or not covering every
    /// file the promotion changes). Nothing was changed.
    #[error("not permitted by lock authority: {0}")]
    LockNotPermitted(String),

    /// Exit 12: canonical advanced past the candidate's target (DD-I4). The
    /// candidate is stale; canonical was not changed.
    #[error("canonical advanced: {0}")]
    CanonicalAdvanced(String),

    /// Internal signal: the session is locked by a long-running operation.
    /// Surfaces as exit 6 if it ever reaches the user.
    #[error("session busy: {0}")]
    Busy(String),
}

impl CliError {
    /// The stable process exit code for this error (§30).
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Usage(_) => 1,
            CliError::PolicyDenied(_) => 2,
            CliError::ContentionWithoutNotice(_) => 3,
            CliError::IsolationFailed(_) => 4,
            CliError::NotFound(_) => 5,
            CliError::InvalidTransition(_) => 6,
            CliError::CorruptMetadata(_) => 7,
            CliError::Internal(_) => 8,
            CliError::NotImplementedPhase(_) => 9,
            CliError::GhostConflict(_) => 10,
            CliError::LockNotPermitted(_) => 11,
            CliError::CanonicalAdvanced(_) => 12,
            CliError::Busy(_) => 6,
        }
    }
}

impl From<GitError> for CliError {
    fn from(err: GitError) -> Self {
        match err {
            GitError::NotARepository(path) => {
                CliError::Usage(format!("not a git repository: {}", path.display()))
            }
            other => CliError::Internal(other.to_string()),
        }
    }
}

impl From<JournalError> for CliError {
    fn from(err: JournalError) -> Self {
        match err {
            JournalError::Corrupt { line, reason } => {
                CliError::CorruptMetadata(format!("journal line {line}: {reason}"))
            }
            JournalError::UnsupportedSchema(detail) => CliError::CorruptMetadata(detail),
            JournalError::Cbor(detail) => CliError::CorruptMetadata(detail),
            other => CliError::Internal(other.to_string()),
        }
    }
}

impl CliError {
    /// Rebuild a typed error from a stable exit code (errors relayed from a
    /// remote Doubledoor keep their contract).
    pub fn from_code(code: u8, message: String) -> Self {
        match code {
            1 => CliError::Usage(message),
            2 => CliError::PolicyDenied(message),
            3 => CliError::ContentionWithoutNotice(message),
            4 => CliError::IsolationFailed(message),
            5 => CliError::NotFound(message),
            6 => CliError::InvalidTransition(message),
            7 => CliError::CorruptMetadata(message),
            10 => CliError::GhostConflict(message),
            11 => CliError::LockNotPermitted(message),
            12 => CliError::CanonicalAdvanced(message),
            _ => CliError::Internal(message),
        }
    }
}

impl From<doubledoor_remote::RemoteError> for CliError {
    fn from(err: doubledoor_remote::RemoteError) -> Self {
        use doubledoor_remote::RemoteError as R;
        match err {
            R::Unknown(n) => CliError::NotFound(format!("remote `{n}`")),
            R::Config(m) => CliError::Usage(m),
            R::Unreachable(m) | R::Protocol(m) => CliError::Internal(format!("remote: {m}")),
            R::Remote(e) => CliError::from_code(e.code, format!("remote: {}", e.message)),
        }
    }
}

impl From<GhostError> for CliError {
    fn from(err: GhostError) -> Self {
        match err {
            GhostError::Git(e) => e.into(),
            GhostError::Corrupt(detail) => CliError::CorruptMetadata(detail),
            GhostError::UnsupportedSchema(e) => CliError::CorruptMetadata(e.to_string()),
            GhostError::SurfaceNotInGhost(_) => CliError::Usage(err.to_string()),
            GhostError::WorkspaceDirty(_) | GhostError::DoesNotApply(_) => {
                CliError::GhostConflict(err.to_string())
            }
            other => CliError::Internal(other.to_string()),
        }
    }
}

impl From<doubledoor_reconcile::ReconcileError> for CliError {
    fn from(err: doubledoor_reconcile::ReconcileError) -> Self {
        use doubledoor_reconcile::ReconcileError as R;
        match err {
            R::Git(e) => e.into(),
            R::Corrupt(d) => CliError::CorruptMetadata(d),
            R::UnknownState(d) => CliError::CorruptMetadata(format!("inconsistent state: {d}")),
            R::CanonicalStateAdvanced { .. } => CliError::CanonicalAdvanced(err.to_string()),
            R::CanonicalRefused(_) | R::ImportConflict(_) => {
                CliError::GhostConflict(err.to_string())
            }
            R::Usage(d) => CliError::Usage(d),
            R::Io(e) => CliError::Internal(e.to_string()),
        }
    }
}

impl From<SurfaceError> for CliError {
    fn from(err: SurfaceError) -> Self {
        match err {
            SurfaceError::Git(e) => e.into(),
            other => CliError::Usage(other.to_string()),
        }
    }
}

impl From<PolicyError> for CliError {
    fn from(err: PolicyError) -> Self {
        match err {
            PolicyError::Denied { reason } => CliError::PolicyDenied(reason.to_string()),
            PolicyError::UnsupportedSchema(detail) => CliError::CorruptMetadata(detail),
            PolicyError::Serde(detail) => CliError::CorruptMetadata(detail.to_string()),
            other => CliError::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(CliError::Usage(String::new()).exit_code(), 1);
        assert_eq!(CliError::PolicyDenied(String::new()).exit_code(), 2);
        assert_eq!(
            CliError::ContentionWithoutNotice(String::new()).exit_code(),
            3
        );
        assert_eq!(CliError::IsolationFailed(String::new()).exit_code(), 4);
        assert_eq!(CliError::NotFound(String::new()).exit_code(), 5);
        assert_eq!(CliError::InvalidTransition(String::new()).exit_code(), 6);
        assert_eq!(CliError::CorruptMetadata(String::new()).exit_code(), 7);
        assert_eq!(CliError::Internal(String::new()).exit_code(), 8);
        assert_eq!(CliError::NotImplementedPhase(String::new()).exit_code(), 9);
        assert_eq!(CliError::GhostConflict(String::new()).exit_code(), 10);
        assert_eq!(CliError::LockNotPermitted(String::new()).exit_code(), 11);
        assert_eq!(CliError::CanonicalAdvanced(String::new()).exit_code(), 12);
    }

    #[test]
    fn ghost_conflicts_map_to_exit_10() {
        let err = CliError::from(GhostError::DoesNotApply("hunk failed".to_string()));
        assert_eq!(err.exit_code(), 10);
        let err = CliError::from(GhostError::WorkspaceDirty(vec!["M a.rs".to_string()]));
        assert_eq!(err.exit_code(), 10);
    }

    #[test]
    fn git_not_a_repository_maps_to_usage() {
        let err = CliError::from(GitError::NotARepository(std::path::PathBuf::from("/x")));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn journal_corruption_maps_to_corrupt_metadata() {
        let err = CliError::from(JournalError::Corrupt {
            line: 3,
            reason: "bad json".to_string(),
        });
        assert_eq!(err.exit_code(), 7);
    }
}
