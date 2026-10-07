//! Phase IV commands (§16, §22, §23): `reconcile`, `validate`, `candidate`,
//! `discard`, `promote`.
//!
//! Orchestration only: candidate construction, merging, and CAS promotion
//! live in `doubledoor-reconcile`; the gates in `doubledoor-validate`;
//! lease checks in `doubledoor-locksmith`. The ordering rules enforced here:
//!
//! - the session is frozen (its lock held) for the whole of
//!   reconcile → validate and of promotion (§16 step 1);
//! - every lifecycle transition is journalled before it is applied, and
//!   candidate/validation records are persisted before the transition that
//!   depends on them;
//! - canonical is touched only by `promote`, only as a fast-forward from
//!   exactly the candidate's target (DD-I4), and only with a Locksmith lease
//!   covering every file it changes (DD-I6);
//! - an interrupted reconcile/validate/promote is resolved by `recover`
//!   from the journal and canonical `HEAD`, never guessed (§24, §43).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use doubledoor_core::evidence::{EvidenceBundle, UnifiedVerdict};
use doubledoor_core::{CandidateId, DoorEvent, DoorSession, DoorState};
use doubledoor_evidence::EvidenceConfig;
use doubledoor_ghost::GhostStore;
use doubledoor_git::Git;
use doubledoor_journal::{
    self as journal, EventLog, Journal, JournalEventKind, JournalRecord, SessionLock,
};
use doubledoor_protocol::StreamEvent;
use doubledoor_reconcile::candidate::snapshot_workspace;
use doubledoor_reconcile::compat::{compatibility, CompatResult, Located};
use doubledoor_reconcile::merge::Mode;
use doubledoor_reconcile::{
    promote, store, BuildInput, Candidate, CandidateStatus, ReconcileError,
};
use doubledoor_surface::semantic::analyze_workspace;
use doubledoor_validate::{ValidationConfig, ValidationReport};

use crate::commands::{parse_session_id, resolve_git, session_dir, store_root};
use crate::error::CliError;
use crate::Cli;

/// Persisted next to a candidate: its validation report and the §52
/// compatibility of the session's retained code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRecord {
    pub candidate: CandidateId,
    pub report: ValidationReport,
    pub compatibility: Vec<CompatResult>,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Journal a lifecycle transition, apply it, and publish it.
pub(crate) fn step(
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    event: DoorEvent,
    detail: impl Into<String>,
) -> Result<(), CliError> {
    let from = session.state;
    let to = from
        .successor(event)
        .map_err(|e| CliError::InvalidTransition(e.to_string()))?;
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from,
        to,
        event: JournalEventKind::Transition {
            event,
            detail: detail.into(),
        },
    })?;
    session.state = to;
    events.append(StreamEvent::SessionState {
        session: session.id,
        from,
        to,
    })?;
    Ok(())
}

fn record(
    jrn: &mut Journal,
    session: &DoorSession,
    event: JournalEventKind,
) -> Result<(), CliError> {
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from: session.state,
        to: session.state,
        event,
    })?;
    Ok(())
}

/// The most recently recorded candidate of a session.
pub(crate) fn latest_candidate(dir: &Path) -> Result<Option<Candidate>, CliError> {
    let records = journal::replay(dir)?.records;
    let id = records.iter().rev().find_map(|r| match &r.event {
        JournalEventKind::CandidateRecorded { candidate, .. } => Some(*candidate),
        _ => None,
    });
    id.map(|id| store::load(dir, &id).map_err(CliError::from))
        .transpose()
}

/// Set the latest candidate's status (no-op without a candidate).
pub(crate) fn mark_latest(dir: &Path, status: CandidateStatus) -> Result<(), CliError> {
    if let Some(mut c) = latest_candidate(dir)? {
        c.status = status;
        store::save(dir, &c)?;
    }
    Ok(())
}

fn candidates_built(dir: &Path) -> Result<usize, CliError> {
    Ok(journal::replay(dir)?
        .records
        .iter()
        .filter(|r| matches!(r.event, JournalEventKind::CandidateRecorded { .. }))
        .count())
}

/// Resolve a session, take its lock without waiting (a long validation run
/// holds it; waiting silently would look like a hang), and load it.
fn open_locked(cli: &Cli, id: &str) -> Result<(Git, PathBuf, SessionLock, DoorSession), CliError> {
    let git = resolve_git(cli)?;
    let id = parse_session_id(id)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    let lock = SessionLock::try_acquire(&dir)?.ok_or_else(|| {
        CliError::InvalidTransition(format!(
            "session {id} is busy (another reconcile, validate, or promote holds it)"
        ))
    })?;
    let session = journal::load_metadata(&dir)?;
    Ok((git, dir, lock, session))
}

fn workspace_of(session: &DoorSession) -> Result<PathBuf, CliError> {
    session
        .workspace
        .as_ref()
        .filter(|w| w.path.is_dir())
        .map(|w| w.path.clone())
        .ok_or_else(|| {
            CliError::InvalidTransition(format!(
                "session {} has no workspace (run `doubledoor recover`)",
                session.id
            ))
        })
}

// ---------------------------------------------------------------------------
// reconcile / validate
// ---------------------------------------------------------------------------

/// `doubledoor reconcile <session> [--converge]`.
pub fn reconcile(cli: &Cli, id: &str, converge: bool) -> Result<(), CliError> {
    // Observe first (it takes and releases the lock itself): a candidate is
    // only as fresh as the ghosts the actor has seen.
    {
        let git = resolve_git(cli)?;
        let sid = parse_session_id(id)?;
        let dir = session_dir(&git, &sid);
        if !dir.is_dir() {
            return Err(CliError::NotFound(sid.to_string()));
        }
        crate::ghosts::observe_dir(&git, &dir)?;
    }
    let (git, dir, _lock, mut session) = open_locked(cli, id)?;
    if session.state != DoorState::Active {
        let hint = match session.state {
            DoorState::GhostPending | DoorState::Assessing => {
                "assess the pending ghosts first (`doubledoor ghosts`)"
            }
            DoorState::Ready | DoorState::ValidationFailed => {
                "promote or discard the current candidate first"
            }
            _ => "only ACTIVE sessions can be reconciled",
        };
        return Err(CliError::InvalidTransition(format!(
            "session {} is {}: {hint}",
            session.id, session.state
        )));
    }
    let built = candidates_built(&dir)?;
    if built >= session.policy.max_reconciliation_attempts as usize {
        return Err(CliError::PolicyDenied(format!(
            "session {} has built {built} candidate(s); the policy allows {} (§28)",
            session.id, session.policy.max_reconciliation_attempts
        )));
    }
    let ws = workspace_of(&session)?;
    let mode = if converge {
        Mode::Converge
    } else {
        Mode::Speculative
    };
    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;

    step(
        &mut jrn,
        &events,
        &mut session,
        DoorEvent::ReconcileBegun,
        format!("{mode:?} mode"),
    )?;
    journal::save_metadata(&dir, &session)?;

    let scratch = dir.join("scratch");
    let ws_git = Git::new(ws.clone());
    let semantics = analyze_workspace(&ws_git, &session.base.commit).ok();
    let ghosts = GhostStore::new(&git, &session.id)?.list()?;
    let records = journal::replay(&dir)?.records;
    let decisions = journal::ghost_decisions(&records);
    let built = doubledoor_reconcile::build(&BuildInput {
        canonical: &git,
        workspace: &ws_git,
        session: &session,
        ghosts: &ghosts,
        decisions: &decisions,
        semantics: semantics.as_ref(),
        mode,
        scratch: &scratch,
    });
    let _ = std::fs::remove_dir_all(&scratch);
    let candidate = match built {
        Ok(c) => c,
        Err(err) => {
            step(
                &mut jrn,
                &events,
                &mut session,
                DoorEvent::ReconcileAborted,
                err.to_string(),
            )?;
            journal::save_metadata(&dir, &session)?;
            return Err(err.into());
        }
    };
    store::save(&dir, &candidate)?;
    record(
        &mut jrn,
        &session,
        JournalEventKind::CandidateRecorded {
            candidate: candidate.id,
            commit: candidate.commit.clone(),
            target: candidate.target.clone(),
            promotable: candidate.promotable,
        },
    )?;
    events.append(StreamEvent::CandidateBuilt {
        session: session.id,
        candidate: candidate.id,
        commit: candidate.commit.clone(),
        promotable: candidate.promotable,
        contested: candidate
            .contested
            .iter()
            .map(|c| c.surface.clone())
            .collect(),
    })?;
    step(
        &mut jrn,
        &events,
        &mut session,
        DoorEvent::CandidateBuilt,
        candidate.id.to_string(),
    )?;
    journal::save_metadata(&dir, &session)?;

    let validation = run_validation(&git, &dir, &mut jrn, &events, &mut session, candidate)?;
    render(cli, &dir, &session, validation)
}

/// `doubledoor validate <session>`: re-run the gates for the current
/// candidate (READY or VALIDATION_FAILED).
pub fn validate(cli: &Cli, id: &str) -> Result<(), CliError> {
    let (git, dir, _lock, mut session) = open_locked(cli, id)?;
    let candidate = latest_candidate(&dir)?.ok_or_else(|| {
        CliError::InvalidTransition(format!("session {} has no candidate", session.id))
    })?;
    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;
    step(
        &mut jrn,
        &events,
        &mut session,
        DoorEvent::ValidationBegun,
        candidate.id.to_string(),
    )?;
    journal::save_metadata(&dir, &session)?;
    let v = run_validation(&git, &dir, &mut jrn, &events, &mut session, candidate)?;
    render(cli, &dir, &session, v)
}

fn run_validation(
    git: &Git,
    dir: &Path,
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    mut candidate: Candidate,
) -> Result<ValidationRecord, CliError> {
    let cfg = match ValidationConfig::load(&store_root(git)) {
        Ok(c) => c,
        Err(e) => {
            abort(dir, jrn, events, session, &e.to_string())?;
            return Err(CliError::Usage(e.to_string()));
        }
    };
    let budget = Duration::from_secs(session.policy.max_validation_runtime_secs);
    let report = match doubledoor_validate::run(
        git,
        &candidate.commit,
        &candidate.target,
        &dir.join("validate"),
        &dir.join("target"),
        &cfg,
        budget,
    ) {
        Ok(r) => r,
        Err(e) => {
            abort(dir, jrn, events, session, &e.to_string())?;
            return Err(CliError::Internal(format!("validation could not run: {e}")));
        }
    };
    let located: Vec<Located> = report
        .diagnostics
        .iter()
        .map(|d| Located {
            file: d.file.clone(),
            line: d.line,
            message: d.message.clone(),
        })
        .collect();
    let compat = compatibility(git, &candidate, &located, report.compiled);
    let rec = ValidationRecord {
        candidate: candidate.id,
        report,
        compatibility: compat,
    };
    store::save_json(
        &doubledoor_reconcile::candidate::candidate_dir(dir),
        &format!("{}.validation", candidate.id),
        &rec,
    )?;
    candidate.status = CandidateStatus::Validated {
        passed: rec.report.passed,
    };
    store::save(dir, &candidate)?;
    record(
        jrn,
        session,
        JournalEventKind::ValidationRecorded {
            candidate: candidate.id,
            passed: rec.report.passed,
        },
    )?;
    events.append(StreamEvent::CandidateValidated {
        session: session.id,
        candidate: candidate.id,
        passed: rec.report.passed,
    })?;
    // Phase V: the unified evidence bundle (Doubledoor's own evidence plus
    // any installed ELCI providers), persisted before the state change.
    let bundle = collect_evidence(git, dir, session, &candidate, &rec);
    store::save_json(
        &doubledoor_reconcile::candidate::candidate_dir(dir),
        &format!("{}.evidence", candidate.id),
        &bundle,
    )?;
    record(
        jrn,
        session,
        JournalEventKind::EvidenceRecorded {
            candidate: candidate.id,
            verdict: format!("{:?}", bundle.assessment.verdict).to_lowercase(),
        },
    )?;

    let (event, detail) = if rec.report.passed {
        (
            DoorEvent::GatesPassed,
            "all required gates passed".to_string(),
        )
    } else {
        let failed = rec
            .report
            .gates
            .iter()
            .find(|g| g.required && !g.status.passed())
            .map(|g| g.name.clone())
            .unwrap_or_default();
        (
            DoorEvent::GatesFailed,
            format!("gate `{failed}` did not pass"),
        )
    };
    step(jrn, events, session, event, detail)?;
    journal::save_metadata(dir, session)?;
    Ok(rec)
}

fn collect_evidence(
    git: &Git,
    dir: &Path,
    session: &DoorSession,
    candidate: &Candidate,
    rec: &ValidationRecord,
) -> EvidenceBundle {
    let config = match EvidenceConfig::load(&store_root(git)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{} evidence config ignored ({e}); using defaults",
                crate::style::warn_prefix()
            );
            EvidenceConfig::default()
        }
    };
    let work = dir.join("evidence");
    let logs = dir.join("validate").join("logs");
    let bundle = doubledoor_evidence::collect(&doubledoor_evidence::Context {
        git,
        candidate,
        validation: Some(&rec.report),
        compatibility: &rec.compatibility,
        validation_logs: Some(&logs),
        intent: &session.intent.summary,
        work: &work,
        config: &config,
    });
    let _ = std::fs::remove_dir_all(work.join("tree"));
    let _ = std::fs::remove_dir_all(work.join("staged"));
    bundle
}

pub(crate) fn load_evidence(dir: &Path, c: &Candidate) -> Option<EvidenceBundle> {
    store::load_json(
        &doubledoor_reconcile::candidate::candidate_dir(dir),
        &format!("{}.evidence", c.id),
    )
    .ok()
}

/// Validation stopped without a result: the candidate is incomplete and the
/// session resumes (§43 — never presented as a completed validation).
fn abort(
    dir: &Path,
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    reason: &str,
) -> Result<(), CliError> {
    mark_latest(
        dir,
        CandidateStatus::Incomplete {
            reason: reason.to_string(),
        },
    )?;
    step(
        jrn,
        events,
        session,
        DoorEvent::ReconcileAborted,
        reason.to_string(),
    )?;
    journal::save_metadata(dir, session)?;
    Ok(())
}

fn load_validation(dir: &Path, c: &Candidate) -> Option<ValidationRecord> {
    store::load_json(
        &doubledoor_reconcile::candidate::candidate_dir(dir),
        &format!("{}.validation", c.id),
    )
    .ok()
}

fn render(
    cli: &Cli,
    dir: &Path,
    session: &DoorSession,
    v: ValidationRecord,
) -> Result<(), CliError> {
    let c =
        latest_candidate(dir)?.ok_or_else(|| CliError::Internal("candidate vanished".into()))?;
    let e = load_evidence(dir, &c);
    crate::output::render_candidate(cli.format, session, &c, Some(&v), e.as_ref())
}

/// `doubledoor candidate <session>`: the latest candidate and its validation.
pub fn show(cli: &Cli, id: &str) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let sid = parse_session_id(id)?;
    let dir = session_dir(&git, &sid);
    if !dir.is_dir() {
        return Err(CliError::NotFound(sid.to_string()));
    }
    let session = journal::load_metadata(&dir)?;
    let c = latest_candidate(&dir)?
        .ok_or_else(|| CliError::NotFound(format!("no candidate for session {sid}")))?;
    let v = load_validation(&dir, &c);
    let e = load_evidence(&dir, &c);
    crate::output::render_candidate(cli.format, &session, &c, v.as_ref(), e.as_ref())
}

/// `doubledoor discard <session>`: set the candidate aside, resume work.
pub fn discard(cli: &Cli, id: &str) -> Result<(), CliError> {
    let (git, dir, _lock, mut session) = open_locked(cli, id)?;
    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;
    step(
        &mut jrn,
        &events,
        &mut session,
        DoorEvent::CandidateDiscarded,
        "discarded by the actor",
    )?;
    mark_latest(&dir, CandidateStatus::Discarded)?;
    journal::save_metadata(&dir, &session)?;
    crate::output::render_open(cli.format, &crate::commands::summary_of(&session))
}

// ---------------------------------------------------------------------------
// promote
// ---------------------------------------------------------------------------

/// `doubledoor promote <session> --lease <json>`.
pub fn promote(cli: &Cli, id: &str, lease: &Path) -> Result<(), CliError> {
    let (git, dir, lock, mut session) = open_locked(cli, id)?;
    if session.state != DoorState::Ready {
        return Err(CliError::InvalidTransition(format!(
            "session {} is {}; only a READY candidate (validated) can be promoted",
            session.id, session.state
        )));
    }
    let mut c = latest_candidate(&dir)?.ok_or_else(|| {
        CliError::InvalidTransition(format!("session {} has no candidate", session.id))
    })?;
    // §23: validation gates pass.
    if c.status != (CandidateStatus::Validated { passed: true }) {
        return Err(CliError::InvalidTransition(format!(
            "candidate {} is {:?}, not validated",
            c.id, c.status
        )));
    }
    // §23: required GhostDiff decisions resolved.
    if !c.promotable {
        return Err(CliError::InvalidTransition(format!(
            "candidate {} is not promotable: {}",
            c.id,
            c.blocking.join("; ")
        )));
    }
    // Phase V: deterministic evidence demonstrating a problem blocks.
    if let Some(b) = load_evidence(&dir, &c) {
        if b.assessment.verdict == UnifiedVerdict::Blocked {
            return Err(CliError::InvalidTransition(format!(
                "candidate {} is BLOCKED by deterministic evidence: {}",
                c.id,
                b.assessment.rationale.first().cloned().unwrap_or_default()
            )));
        }
    }
    // §23: candidate derived from known state — the session's work has not
    // moved on since the candidate captured it.
    let ws = workspace_of(&session)?;
    let scratch = dir.join("scratch");
    let (_, tree_now) = snapshot_workspace(&Git::new(ws), &scratch, session.actor.as_str())
        .map_err(CliError::from)?;
    let _ = std::fs::remove_dir_all(&scratch);
    if tree_now != c.snapshot_tree {
        return Err(CliError::InvalidTransition(format!(
            "the workspace changed since candidate {} was built; run `doubledoor reconcile` again",
            c.id
        )));
    }
    // §23: Locksmith permits the target mutation (DD-I6: received, never forged).
    let json = std::fs::read_to_string(lease).map_err(|e| {
        CliError::LockNotPermitted(format!("cannot read lease {}: {e}", lease.display()))
    })?;
    let grant = doubledoor_locksmith::parse_grant(&json).map_err(|e| {
        CliError::LockNotPermitted(format!("unparsable lease {}: {e}", lease.display()))
    })?;
    doubledoor_locksmith::authorize_mutation(&grant, &session.actor, &c.promotes_files)
        .map_err(|e| CliError::LockNotPermitted(e.to_string()))?;

    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;
    // DD-I4 pre-check (promote re-checks atomically).
    let head = git.head_oid()?;
    if head != c.target {
        return stale(&git, &dir, lock, &mut jrn, &events, &mut session, &c, &head);
    }
    step(
        &mut jrn,
        &events,
        &mut session,
        DoorEvent::PromotionBegun,
        c.id.to_string(),
    )?;
    journal::save_metadata(&dir, &session)?;

    match promote::promote(&git, &c) {
        Ok(new_head) => {
            complete(
                &git,
                &dir,
                &mut jrn,
                &events,
                &mut session,
                &mut c,
                &new_head,
            )?;
            drop(lock);
            crate::output::render_promotion(cli.format, &session, &c, &new_head)
        }
        Err(ReconcileError::CanonicalStateAdvanced { actual, .. }) => stale(
            &git,
            &dir,
            lock,
            &mut jrn,
            &events,
            &mut session,
            &c,
            &actual,
        ),
        Err(err) => {
            step(
                &mut jrn,
                &events,
                &mut session,
                DoorEvent::PromotionAborted,
                err.to_string(),
            )?;
            journal::save_metadata(&dir, &session)?;
            Err(err.into())
        }
    }
}

/// A promotion finished: journal it, then finish (full) or resume (partial).
pub(crate) fn complete(
    git: &Git,
    dir: &Path,
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    c: &mut Candidate,
    new_head: &doubledoor_core::GitOid,
) -> Result<(), CliError> {
    let partial = c.is_partial();
    record(
        jrn,
        session,
        JournalEventKind::CanonicalPromoted {
            candidate: c.id,
            from: c.target.clone(),
            to: new_head.clone(),
            partial,
        },
    )?;
    c.status = CandidateStatus::Promoted { partial };
    store::save(dir, c)?;
    events.append(StreamEvent::CandidatePromoted {
        session: session.id,
        candidate: c.id,
        from: c.target.clone(),
        to: new_head.clone(),
        partial,
    })?;
    // The promoted commit is the session's own work, not a ghost of it.
    record(
        jrn,
        session,
        JournalEventKind::GhostCursorAdvanced {
            to: new_head.clone(),
        },
    )?;
    session.ghost_cursor.commit = new_head.clone();
    if partial {
        step(
            jrn,
            events,
            session,
            DoorEvent::PartialPromotionCompleted,
            format!(
                "contested surfaces stay isolated: {}",
                c.contested
                    .iter()
                    .map(|x| x.surface.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )?;
        journal::save_metadata(dir, session)?;
    } else {
        step(
            jrn,
            events,
            session,
            DoorEvent::PromotionCompleted,
            c.id.to_string(),
        )?;
        journal::save_metadata(dir, session)?;
        // The work is canonical now; the speculative workspace is retired.
        if let Some(ws) = session.workspace.clone() {
            if let Some(p) = ws.path.to_str() {
                let _ = git.run_git(&["worktree", "remove", "--force", p]);
            }
            let _ = std::fs::remove_dir_all(&ws.path);
            session.workspace = None;
            journal::save_metadata(dir, session)?;
        }
    }
    Ok(())
}

/// Canonical advanced past the candidate: mark it stale, return the session
/// to ACTIVE, and observe the new commits (which may require assessment).
#[allow(clippy::too_many_arguments)]
fn stale(
    git: &Git,
    dir: &Path,
    lock: SessionLock,
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    c: &Candidate,
    actual: &doubledoor_core::GitOid,
) -> Result<(), CliError> {
    mark_latest(dir, CandidateStatus::Stale)?;
    step(
        jrn,
        events,
        session,
        DoorEvent::CanonicalAdvanced,
        format!(
            "canonical is {actual}, candidate {} targets {}",
            c.id, c.target
        ),
    )?;
    journal::save_metadata(dir, session)?;
    drop(lock);
    crate::ghosts::observe_dir(git, dir)?;
    Err(CliError::CanonicalAdvanced(format!(
        "canonical HEAD is {actual}, but candidate {} was built on {}; the candidate is stale — assess the new ghosts and reconcile again",
        c.id, c.target
    )))
}
