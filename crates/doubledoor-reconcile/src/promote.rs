//! Compare-and-swap promotion (§23, DD-I4).
//!
//! A candidate's parent is the canonical `HEAD` it was built against, so
//! promotion is a fast-forward of the checked-out canonical branch from
//! exactly that commit to the candidate:
//!
//! 1. `HEAD` must equal the candidate's target, else
//!    [`ReconcileError::CanonicalStateAdvanced`] — nothing is touched;
//! 2. `git merge --ff-only <candidate>` performs the checkout and the ref
//!    update; Git updates the ref in a transaction that verifies the old
//!    value it read, and refuses anything but a fast-forward, so a
//!    concurrent canonical commit makes it fail rather than be overwritten;
//! 3. `HEAD` must now equal the candidate.
//!
//! A crash between 1 and 3 is resolved by [`classify_after_crash`]: the
//! canonical `HEAD` is either the candidate (promoted), the target (not
//! promoted), or something else (canonical advanced). The ref is never left
//! half-updated: Git's ref update is atomic.

use doubledoor_core::GitOid;
use doubledoor_git::Git;

use crate::candidate::Candidate;
use crate::error::ReconcileError;

/// What a crashed promotion turned out to have done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashOutcome {
    Promoted,
    NotPromoted,
    CanonicalAdvanced,
}

/// Promote `c` onto the canonical worktree's current branch.
pub fn promote(git: &Git, c: &Candidate) -> Result<GitOid, ReconcileError> {
    verify_derivation(git, c)?;
    let head = git.head_oid()?;
    if head != c.target {
        return Err(ReconcileError::CanonicalStateAdvanced {
            expected: c.target.clone(),
            actual: head,
        });
    }
    if let Err(e) = git.run_git(&["merge", "--ff-only", "--no-edit", "-q", c.commit.as_str()]) {
        let now = git.head_oid()?;
        return Err(if now != c.target {
            ReconcileError::CanonicalStateAdvanced {
                expected: c.target.clone(),
                actual: now,
            }
        } else {
            ReconcileError::CanonicalRefused(e.to_string())
        });
    }
    let now = git.head_oid()?;
    if now != c.commit {
        return Err(ReconcileError::UnknownState(format!(
            "after promotion canonical HEAD is {now}, expected {}",
            c.commit
        )));
    }
    Ok(now)
}

/// §23 "candidate derived from known state": the candidate commit exists,
/// its only parent is the recorded target, and its tree is the recorded one.
pub fn verify_derivation(git: &Git, c: &Candidate) -> Result<(), ReconcileError> {
    let parents = git.run_git(&["show", "-s", "--format=%P", c.commit.as_str()])?;
    let tree = git.run_git(&["rev-parse", &format!("{}^{{tree}}", c.commit)])?;
    if parents.trim() != c.target.as_str() || tree.trim() != c.tree.as_str() {
        return Err(ReconcileError::UnknownState(format!(
            "candidate {} does not match its record (parent {parents}, tree {tree})",
            c.commit
        )));
    }
    Ok(())
}

/// After a crash in PROMOTING, decide what happened from canonical `HEAD`.
pub fn classify_after_crash(git: &Git, c: &Candidate) -> Result<CrashOutcome, ReconcileError> {
    let head = git.head_oid()?;
    Ok(if head == c.commit {
        CrashOutcome::Promoted
    } else if head == c.target {
        CrashOutcome::NotPromoted
    } else {
        CrashOutcome::CanonicalAdvanced
    })
}
