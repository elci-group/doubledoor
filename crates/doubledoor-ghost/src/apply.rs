//! ACCEPT (§14/§15): bring a ghost's changes into a session's speculative
//! workspace — only when the actor asks, never automatically (§46 exit
//! criterion, DD-I2).
//!
//! Acceptance may be surface-selective (§15): the actor names the subset of
//! the ghost's affected file surfaces to import and keeps its own version of
//! the rest. The selected delta is extracted from the ghost's source commit,
//! kept on disk as evidence (DD-I5), checked, applied to the workspace index
//! and tree, and committed as a *new* workspace commit — the ghost's
//! historical commit is never rewritten or cherry-picked wholesale (§15).
//!
//! Safety rules:
//! - the selected paths must be clean in the workspace, so the actor's own
//!   uncommitted edits are never folded into (or clobbered by) the import;
//! - `git apply --check` runs before anything is touched — a delta that does
//!   not apply cleanly leaves the workspace exactly as it was;
//! - the commit is path-limited, so unrelated staged work stays staged.

use std::fs;
use std::path::{Path, PathBuf};

use doubledoor_core::{GitOid, SurfaceId};
use doubledoor_git::Git;

use crate::error::GhostError;
use crate::ghost::GhostDiff;

/// The file-surface id prefix produced by `doubledoor-surface`.
const FILE_PREFIX: &str = "file:";

/// What an accepted ghost did to the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptOutcome {
    /// The surfaces whose changes were imported.
    pub surfaces: Vec<SurfaceId>,
    /// The new workspace commit, or `None` when the selected delta was empty
    /// (e.g. the changes were already present) and nothing was committed.
    pub applied_commit: Option<GitOid>,
    /// Where the applied patch was preserved as evidence.
    pub patch_path: PathBuf,
}

/// Apply `ghost` (restricted to `selected` surfaces, or all affected surfaces
/// when `None`) to the worktree at `workspace`. `canonical` is the repository
/// the ghost was observed in; `patch_path` is where the extracted patch is
/// preserved.
pub fn accept_into_workspace(
    canonical: &Git,
    workspace: &Path,
    ghost: &GhostDiff,
    selected: Option<&[SurfaceId]>,
    patch_path: &Path,
) -> Result<AcceptOutcome, GhostError> {
    let surfaces: Vec<SurfaceId> = match selected {
        Some(sel) => {
            for s in sel {
                if !ghost.affected_surfaces.contains(s) {
                    return Err(GhostError::SurfaceNotInGhost(s.as_str().to_string()));
                }
            }
            let mut v = sel.to_vec();
            v.sort();
            v.dedup();
            v
        }
        None => ghost.affected_surfaces.iter().cloned().collect(),
    };
    let paths: Vec<&str> = surfaces
        .iter()
        .map(|s| {
            s.as_str().strip_prefix(FILE_PREFIX).ok_or_else(|| {
                GhostError::UnexpectedOutput(format!("not a file surface: {}", s.as_str()))
            })
        })
        .collect::<Result<_, _>>()?;

    let ws = Git::new(workspace.to_path_buf());
    if !paths.is_empty() {
        let mut status_args = vec!["status", "--porcelain", "--"];
        status_args.extend_from_slice(&paths);
        let dirty = ws.run_git(&status_args)?;
        if !dirty.is_empty() {
            return Err(GhostError::WorkspaceDirty(
                dirty.lines().map(|l| l.trim().to_string()).collect(),
            ));
        }
    }

    // Extract the selected delta from the ghost's own commit.
    let root = ghost.parent_commit.as_str().bytes().all(|b| b == b'0');
    let mut diff_args = vec![
        "diff-tree",
        "-p",
        "--binary",
        "--full-index",
        "--no-commit-id",
    ];
    if root {
        diff_args.push("--root");
    } else {
        diff_args.push(ghost.parent_commit.as_str());
    }
    diff_args.push(ghost.source_commit.as_str());
    diff_args.push("--");
    diff_args.extend_from_slice(&paths);
    let patch = if paths.is_empty() {
        Vec::new()
    } else {
        canonical.run_git_bytes(&diff_args)?
    };

    if let Some(parent) = patch_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(patch_path, &patch)?;
    if patch.is_empty() {
        return Ok(AcceptOutcome {
            surfaces,
            applied_commit: None,
            patch_path: patch_path.to_path_buf(),
        });
    }
    let patch_arg = patch_path
        .to_str()
        .ok_or_else(|| GhostError::UnexpectedOutput("patch path is not UTF-8".to_string()))?;

    // Check first: a delta that does not apply leaves the workspace untouched.
    ws.run_git(&["apply", "--index", "--check", patch_arg])
        .map_err(|e| GhostError::DoesNotApply(e.to_string()))?;
    ws.run_git(&["apply", "--index", patch_arg])
        .map_err(|e| GhostError::DoesNotApply(e.to_string()))?;

    let author = canonical.run_git(&[
        "show",
        "-s",
        "--format=%an <%ae>",
        ghost.source_commit.as_str(),
    ])?;
    let message = format!(
        "doubledoor: accept ghost {}\n\nImports {} surface(s) from canonical commit {}.\n\nDd-Ghost: {}\nDd-Source-Commit: {}\n",
        ghost.id,
        surfaces.len(),
        ghost.source_commit,
        ghost.id,
        ghost.source_commit,
    );
    let mut commit_args = vec![
        "commit",
        "-q",
        "--no-verify",
        "--author",
        author.as_str(),
        "-m",
        message.as_str(),
        "--",
    ];
    commit_args.extend_from_slice(&paths);
    if let Err(err) = ws.run_git(&commit_args) {
        // Undo the index/tree import so a failed accept leaves no trace.
        if let Err(undo) = ws.run_git(&["apply", "--index", "-R", patch_arg]) {
            tracing::warn!(error = %undo, "could not reverse a failed ghost import");
        }
        return Err(err.into());
    }
    Ok(AcceptOutcome {
        surfaces,
        applied_commit: Some(ws.head_oid()?),
        patch_path: patch_path.to_path_buf(),
    })
}
