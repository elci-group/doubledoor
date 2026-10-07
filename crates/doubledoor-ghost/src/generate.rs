//! Ghost observation (§46, Phase II): walk the commits a session has not yet
//! seen, build a [`GhostDiff`] for each, classify it against the session's
//! read/write/dependency sets, and persist the results.
//!
//! Cursor discipline: `generate` observes `session.ghost_cursor..HEAD` and
//! reports the new cursor (always `HEAD`) in its return value — **advancing
//! the cursor on the session is the caller's job**, so a crashed run never
//! records progress it did not finish persisting. Re-runs are idempotent: a
//! commit that already has a persisted ghost in the store is skipped, so a
//! crash between saving ghosts and advancing the cursor never duplicates one.

use doubledoor_core::{ActorId, DoorSession, GhostDiffId, GitOid, IntentDescriptor};
use doubledoor_git::Git;
use doubledoor_surface::semantic::{dependency_delta, semantic_delta, SessionSemantics};
use doubledoor_surface::{parse_name_status, SurfaceDelta};

use crate::assess::assess;
use crate::diff::parse_unified_diff;
use crate::error::GhostError;
use crate::ghost::GhostDiff;
use crate::store::GhostStore;

/// The result of one observation run.
#[derive(Debug, Clone)]
pub struct GhostObservation {
    /// One ghost per observed commit, in commit (oldest-first) order.
    pub ghosts: Vec<GhostDiff>,
    /// The cursor the session should be advanced to on success: `HEAD`.
    pub new_cursor: GitOid,
}

/// Observe every commit in `session.ghost_cursor..HEAD` (oldest first), and
/// persist a classified [`GhostDiff`] per commit into `store`.
///
/// The session's read/write/dependency sets must already be populated (the
/// CLI registers them when the actor's plan is known); sessions with empty
/// sets classify everything `Independent` — honest, since an actor with no
/// declared surfaces overlaps with nothing.
pub fn generate(
    git: &Git,
    session: &DoorSession,
    store: &GhostStore,
) -> Result<GhostObservation, GhostError> {
    generate_with(git, session, store, None)
}

/// [`generate`] with the session's derived symbol-level footprint (Phase
/// III), from `doubledoor_surface::semantic::analyze_workspace`. With
/// `None`, symbol-level breaking changes are assessed `Unknown` (they cannot
/// be ruled out), and everything else as in Phase II.
pub fn generate_with(
    git: &Git,
    session: &DoorSession,
    store: &GhostStore,
    semantics: Option<&SessionSemantics>,
) -> Result<GhostObservation, GhostError> {
    let head = git.head_oid()?;
    // Metadata written before Phase II carries the unborn cursor; the base
    // commit is the honest starting point for such sessions.
    let cursor = if session.ghost_cursor.is_unborn() {
        session.base.commit.clone()
    } else {
        session.ghost_cursor.commit.clone()
    };
    if cursor == head {
        return Ok(GhostObservation {
            ghosts: Vec::new(),
            new_cursor: head,
        });
    }
    let range = format!("{cursor}..{head}");
    tracing::info!(session = %session.id, %range, "observing ghost commits");

    // %x1f (unit separator) delimits the two fields. Trailer placeholders are
    // deliberately NOT part of the walk format: git's %(trailers) expansion
    // emits stray separator lines for commits lacking a trailer, which would
    // corrupt a multi-record parse. Trailers are queried per commit instead
    // (see `trailer_value`). The format rides as one `--format=...` argument:
    // git log rejects a space-separated format value.
    let format = "--format=%H%x1f%an <%ae>";
    let log = git.run_git(&["log", "--reverse", format, &range])?;

    let already: std::collections::HashSet<GitOid> =
        store.list()?.into_iter().map(|g| g.source_commit).collect();
    let mut ghosts = Vec::new();
    for line in log.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(commit) = line.split('\x1f').next() {
            if GitOid::parse(commit).is_ok_and(|c| already.contains(&c)) {
                continue;
            }
        }
        let ghost = observe_commit(git, session, semantics, line, &range)?;
        store.save(&ghost)?;
        ghosts.push(ghost);
    }
    Ok(GhostObservation {
        ghosts,
        new_cursor: head,
    })
}

/// Build (and classify) the ghost for one `git log` output line.
fn observe_commit(
    git: &Git,
    session: &DoorSession,
    semantics: Option<&SessionSemantics>,
    line: &str,
    range: &str,
) -> Result<GhostDiff, GhostError> {
    let mut fields = line.split('\x1f');
    let commit_raw = fields.next().unwrap_or_default();
    let commit = GitOid::parse(commit_raw).map_err(|_| {
        GhostError::UnexpectedOutput(format!("git log returned a non-commit id: {commit_raw}"))
    })?;
    let author = fields.next().unwrap_or_default();
    let actor_trailer = trailer_value(git, &commit, "dd-actor")?;
    let intent_trailer = trailer_value(git, &commit, "dd-intent")?;

    let mut evidence = vec![
        format!("git log --reverse --format=... {range}"),
        format!("commit {commit}"),
    ];

    let parents = git.run_git(&["show", "-s", "--format=%P", commit.as_str()])?;
    let parent = parents.split(' ').next().unwrap_or_default();
    let (parent_commit, diff_args): (GitOid, Vec<&str>) = if parent.is_empty() {
        evidence.push("root commit; delta computed against the empty tree".to_string());
        (null_oid(), vec!["--root"])
    } else {
        let parent = GitOid::parse(parent).map_err(|_| {
            GhostError::UnexpectedOutput(format!(
                "commit {commit} has an invalid parent id {parent}"
            ))
        })?;
        evidence.push(format!("parent {parent}"));
        (parent, Vec::new())
    };

    // Surface delta (name-status) and textual delta (patch) are computed
    // independently; either failing downgrades the classification to Unknown
    // (§12) with the failure recorded in evidence.
    let delta_result = surface_delta(git, &commit, &parent_commit, &diff_args);
    let delta: SurfaceDelta = match &delta_result {
        Ok(delta) => {
            evidence.push(format!(
                "git diff-tree --no-commit-id --name-status -r {} {}",
                parent_commit, commit
            ));
            delta.clone()
        }
        Err(err) => {
            evidence.push(format!("name-status analysis failed: {err}"));
            SurfaceDelta::default()
        }
    };

    let patch = patch_text(git, &commit, &parent_commit, &diff_args);
    let textual = match &patch {
        Ok(text) => {
            evidence.push(format!(
                "git diff-tree -p --no-commit-id {} {}",
                parent_commit, commit
            ));
            parse_unified_diff(text)
        }
        Err(err) => {
            evidence.push(format!("patch retrieval failed: {err}"));
            Err(crate::diff::DiffParseError::Malformed(
                "patch unavailable".to_string(),
            ))
        }
    };
    let analysis_ok = delta_result.is_ok() && textual.is_ok();
    let textual = textual.unwrap_or_default();
    if !analysis_ok {
        evidence.push("analysis incomplete; classification is Unknown (never safe)".to_string());
    }

    let affected_surfaces = delta.affected();

    // Phase III: symbol- and manifest-level deltas of the same commit.
    let changed_paths: Vec<String> = affected_surfaces
        .iter()
        .filter_map(|s| s.as_str().strip_prefix("file:").map(ToString::to_string))
        .collect();
    let parent_arg = (!diff_args.contains(&"--root")).then_some(&parent_commit);
    let mut semantic_ok = true;
    let semantic = match semantic_delta(git, parent_arg, &commit, &changed_paths) {
        Ok(d) => {
            if !d.analyzed_files.is_empty() {
                evidence.push(format!(
                    "symbol analysis (syn AST, syntactic name resolution) of {} Rust file(s): {} symbol change(s)",
                    d.analyzed_files.len(),
                    d.changes.len()
                ));
            }
            for u in &d.unparsed {
                evidence.push(format!(
                    "{} ({}) not analyzed: {}",
                    u.path, u.side, u.reason
                ));
            }
            d
        }
        Err(err) => {
            semantic_ok = false;
            evidence.push(format!("symbol analysis failed: {err}"));
            Default::default()
        }
    };
    let dependencies = dependency_delta(git, parent_arg, &commit, &changed_paths);
    if !dependencies.changes.is_empty() {
        evidence.push(format!(
            "Cargo manifest analysis: {} structural change(s)",
            dependencies.changes.len()
        ));
    }
    match semantics {
        Some(s) => evidence.push(format!(
            "session footprint: {} changed file(s), {} modified symbol(s), {} reference(s)",
            s.changed_files.len(),
            s.modified.len(),
            s.references.len()
        )),
        None => evidence.push("session footprint unavailable".to_string()),
    }

    let assessment = assess(
        session,
        semantics,
        &affected_surfaces,
        &semantic,
        &dependencies,
        analysis_ok && semantic_ok,
    );
    evidence.extend(assessment.notes.iter().cloned());
    let session_sets = session
        .read_set
        .union(&session.write_set)
        .union(&session.dependency_set);
    let intersecting_surfaces = affected_surfaces
        .intersection(&session_sets)
        .union(&assessment.intersecting);
    let classification = assessment.classification;
    let api = semantic.api();

    let source_actor = parse_actor(&actor_trailer).or_else(|| parse_actor(author));
    let intent = if intent_trailer.is_empty() {
        None
    } else {
        Some(IntentDescriptor::new(intent_trailer))
    };

    Ok(GhostDiff {
        id: GhostDiffId::new(),
        session: session.id,
        source_actor,
        source_commit: commit,
        parent_commit,
        intent,
        textual,
        semantic,
        api,
        dependencies,
        assessments: assessment.assessments,
        affected_surfaces,
        intersecting_surfaces,
        invalidated_assumptions: assessment.violations,
        classification,
        evidence,
    })
}

/// `diff-tree --no-commit-id --name-status -r` against the parent (or, for
/// root commits, against the empty tree via `--root`).
fn surface_delta(
    git: &Git,
    commit: &GitOid,
    parent: &GitOid,
    extra: &[&str],
) -> Result<SurfaceDelta, GhostError> {
    let mut args: Vec<&str> = vec!["diff-tree", "--no-commit-id", "--name-status", "-r"];
    args.extend_from_slice(extra);
    if extra.is_empty() {
        args.push(parent.as_str());
    }
    args.push(commit.as_str());
    Ok(parse_name_status(&git.run_git(&args)?)?)
}

/// `diff-tree -p --no-commit-id` against the parent (or empty tree).
fn patch_text(
    git: &Git,
    commit: &GitOid,
    parent: &GitOid,
    extra: &[&str],
) -> Result<String, GhostError> {
    let mut args: Vec<&str> = vec!["diff-tree", "-p", "--no-commit-id"];
    args.extend_from_slice(extra);
    if extra.is_empty() {
        args.push(parent.as_str());
    }
    args.push(commit.as_str());
    Ok(git.run_git(&args)?)
}

/// Map a trailer value or git author identity to an [`ActorId`]; anything
/// that fails id validation yields `None` rather than a fabricated identity.
fn parse_actor(raw: &str) -> Option<ActorId> {
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else {
        ActorId::new(raw).ok()
    }
}

/// The first value of a `key: value` commit trailer (e.g. `dd-actor`), or an
/// empty string when the commit carries none. Queried per commit because
/// `git log`'s `%(trailers)` expansion corrupts multi-record output.
fn trailer_value(git: &Git, commit: &GitOid, key: &str) -> Result<String, GhostError> {
    let format = format!("--format=%(trailers:key={key},valueonly)");
    let out = git.run_git(&["show", "-s", &format, commit.as_str()])?;
    Ok(out
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string())
}

/// The null (all-zeros) object id: the recorded `parent_commit` for root
/// commits, whose delta was computed against the empty tree.
fn null_oid() -> GitOid {
    GitOid::parse(&"0".repeat(40)).expect("40 hex digits")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_actor_prefers_valid_trailers() {
        assert_eq!(parse_actor("agent-99").unwrap().as_str(), "agent-99");
        assert!(parse_actor("bad/actor").is_none());
        assert!(parse_actor("   ").is_none());
        // Git author form maps when it passes id validation.
        assert_eq!(
            parse_actor("Other Actor <other@example.dev>")
                .unwrap()
                .as_str(),
            "Other Actor <other@example.dev>"
        );
    }
}
