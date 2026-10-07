//! Typed errors for ghost observation, classification, and persistence.

use thiserror::Error;

use doubledoor_git::GitError;
use doubledoor_protocol::SchemaVersionError;
use doubledoor_surface::SurfaceError;

/// Failure modes of the ghost layer.
#[derive(Debug, Error)]
pub enum GhostError {
    /// A `git` invocation failed (transparent passthrough of the choke-point
    /// error, argv/code/stderr included).
    #[error(transparent)]
    Git(#[from] GitError),

    /// Surface delta computation failed.
    #[error(transparent)]
    Surface(#[from] SurfaceError),

    /// Filesystem I/O while persisting or loading ghost records.
    #[error("ghost store I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A persisted ghost record is not parseable JSON for the expected shape.
    #[error("ghost record is corrupt: {0}")]
    Corrupt(String),

    /// A persisted ghost record carries a schema version this build does not
    /// understand (§44: rejected explicitly, never silently).
    #[error("ghost record schema incompatible: {0}")]
    UnsupportedSchema(#[from] SchemaVersionError),

    /// Git produced output this layer cannot interpret (e.g. a `git log`
    /// format line without the expected field separators).
    #[error("unexpected git output: {0}")]
    UnexpectedOutput(String),

    /// ACCEPT named a surface the ghost did not touch.
    #[error("surface {0} is not affected by this ghost")]
    SurfaceNotInGhost(String),

    /// ACCEPT would touch paths with uncommitted workspace changes; the
    /// actor's own edits are never folded into an import.
    #[error("workspace has uncommitted changes to the imported paths: {}", .0.join(", "))]
    WorkspaceDirty(Vec<String>),

    /// The ghost's delta does not apply cleanly to the workspace; nothing was
    /// changed. Combining both histories is reconciliation (Phase IV).
    #[error("ghost does not apply cleanly to the workspace: {0}")]
    DoesNotApply(String),
}
