//! Phase II commands (§46 ghost observation): `watch`, `events`, `ghosts`,
//! `accept`, `defer`, `reject`.
//!
//! Orchestration only. Ghost generation, classification, and workspace import
//! live in `doubledoor-ghost`; journal and event-log durability in
//! `doubledoor-journal`; the lifecycle in `doubledoor-core`. The rules this
//! module enforces are ordering rules:
//!
//! - every state change is journalled before it is applied, and the event
//!   stream is written after the journal (the journal is the authority; the
//!   stream is delivery);
//! - observation never touches a workspace — a ghost enters a workspace only
//!   through an explicit `accept` (§46 exit criterion, DD-I2);
//! - a session enters GHOST_PENDING only for ghosts that need assessment
//!   (anything but `independent`; `unknown` is never safe, §12), and returns
//!   to ACTIVE only once every such ghost has an answer.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use doubledoor_core::{
    AutoAcceptLevel, DoorEvent, DoorSession, DoorSessionId, DoorState, GhostDecision, GhostDiffId,
    SurfaceId,
};
use doubledoor_ghost::{
    accept_into_workspace, generate_with, GhostClassification, GhostDiff, GhostStore,
};
use doubledoor_git::Git;
use doubledoor_journal::{
    self as journal, EventCursor, EventLog, Journal, JournalEventKind, JournalRecord, SessionLock,
};
use doubledoor_protocol::{
    AssessmentView, AssumptionView, DecisionView, GhostView, SessionObservation, StreamEvent,
    SurfacesView,
};
use doubledoor_surface::semantic::{analyze_workspace, ChangeKind, SessionSemantics};

use crate::commands::{
    file_surface_arg, parse_session_id, resolve_git, session_dir, sessions_root, store_root,
    surface_strings,
};
use crate::error::CliError;
use crate::Cli;

/// The actor's answer to a ghost, as requested on the command line.
pub enum Choice<'a> {
    /// Accept, optionally restricted to these file surfaces or these Rust
    /// symbols (§15).
    Accept {
        surfaces: &'a [String],
        symbols: &'a [String],
    },
    Defer,
    Reject,
}

/// States in which canonical commits are observed for a session: it holds a
/// workspace and is not frozen mid reconcile/validate/promote.
fn is_observed(state: DoorState) -> bool {
    matches!(
        state,
        DoorState::Active
            | DoorState::GhostPending
            | DoorState::Assessing
            | DoorState::Ready
            | DoorState::ValidationFailed
    )
}

/// Observe one session now (`ghosts`, `reconcile`, stale promotions). A
/// session busy with a long operation is skipped, not waited for.
pub(crate) fn observe_dir(git: &Git, dir: &Path) -> Result<(), CliError> {
    let events = EventLog::new(&store_root(git));
    observe_session(git, &events, dir, &git.head_oid()?).map(|_| ())
}

/// States in which a session's ghosts can be assessed.
fn is_live(state: DoorState) -> bool {
    matches!(
        state,
        DoorState::Active | DoorState::GhostPending | DoorState::Assessing
    )
}

fn record(
    jrn: &mut Journal,
    session: &DoorSession,
    to: DoorState,
    event: JournalEventKind,
) -> Result<(), CliError> {
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from: session.state,
        to,
        event,
    })?;
    Ok(())
}

/// Journal and apply one lifecycle transition, then publish it.
fn transition(
    jrn: &mut Journal,
    events: &EventLog,
    session: &mut DoorSession,
    event: DoorEvent,
    kind: JournalEventKind,
) -> Result<(), CliError> {
    let from = session.state;
    let to = from
        .successor(event)
        .map_err(|e| CliError::InvalidTransition(e.to_string()))?;
    record(jrn, session, to, kind)?;
    session
        .transition(event)
        .map_err(|e| CliError::InvalidTransition(e.to_string()))?;
    events.append(StreamEvent::SessionState {
        session: session.id,
        from,
        to,
    })?;
    Ok(())
}

/// Ghosts that still block the session: they need assessment and have no
/// answer yet (a deferral is an answer — the actor chose to continue).
fn outstanding<'g>(
    ghosts: &'g [GhostDiff],
    decisions: &HashMap<GhostDiffId, GhostDecision>,
) -> Vec<&'g GhostDiff> {
    ghosts
        .iter()
        .filter(|g| g.classification.needs_assessment() && !decisions.contains_key(&g.id))
        .collect()
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// One observation pass for one session: record ghosts for canonical commits
/// past the session's cursor, advance the cursor, and move ACTIVE →
/// GHOST_PENDING when something needs the actor's assessment. The workspace
/// is never touched.
fn observe_session(
    git: &Git,
    events: &EventLog,
    dir: &Path,
    head: &doubledoor_core::GitOid,
) -> Result<Option<SessionObservation>, CliError> {
    let Some(_lock) = SessionLock::try_acquire(dir)? else {
        return Err(CliError::Busy(dir.display().to_string()));
    };
    let mut session = journal::load_metadata(dir)?;
    if !is_observed(session.state) {
        return Ok(None);
    }
    if !session.ghost_cursor.is_unborn() && &session.ghost_cursor.commit == head {
        return Ok(None);
    }

    let store = GhostStore::new(git, &session.id)?;
    let semantics = session_semantics(&session);
    let observation = generate_with(git, &session, &store, semantics.as_ref())?;
    let replay = journal::replay(dir)?;
    let recorded = journal::recorded_ghosts(&replay.records);
    let all = store.list()?;

    // Newly generated ghosts in commit order, then any ghost a crashed run
    // persisted but never journalled.
    let mut to_record: Vec<&GhostDiff> = observation.ghosts.iter().collect();
    for g in &all {
        if !recorded.contains(&g.id) && !to_record.iter().any(|n| n.id == g.id) {
            to_record.push(g);
        }
    }

    let mut jrn = Journal::open(dir)?;
    let mut new_ghosts = Vec::new();
    for g in to_record {
        if recorded.contains(&g.id) {
            continue;
        }
        record(
            &mut jrn,
            &session,
            session.state,
            JournalEventKind::GhostRecorded {
                ghost: g.id,
                source_commit: g.source_commit.clone(),
            },
        )?;
        events.append(StreamEvent::GhostAvailable {
            session: session.id,
            ghost: g.id,
            source_commit: g.source_commit.clone(),
            source_actor: g.source_actor.as_ref().map(|a| a.as_str().to_string()),
            classification: g.classification.wire_name().to_string(),
            intersections: surface_strings(&g.intersecting_surfaces),
            needs_assessment: g.classification.needs_assessment(),
        })?;
        tracing::info!(session = %session.id, ghost = %g.id, class = %g.classification, "ghost recorded");
        new_ghosts.push(g.id);
    }

    if session.ghost_cursor.commit != observation.new_cursor {
        record(
            &mut jrn,
            &session,
            session.state,
            JournalEventKind::GhostCursorAdvanced {
                to: observation.new_cursor.clone(),
            },
        )?;
        session.ghost_cursor.commit = observation.new_cursor.clone();
    }

    // Canonical moved past a candidate: it is stale (§23). A READY session
    // returns to ACTIVE (then to GHOST_PENDING below if anything needs
    // assessment); a failed candidate is simply marked.
    if !to_record_empty(&new_ghosts)
        && matches!(
            session.state,
            DoorState::Ready | DoorState::ValidationFailed
        )
    {
        crate::reconcile::mark_latest(dir, doubledoor_reconcile::CandidateStatus::Stale)?;
        if session.state == DoorState::Ready {
            crate::reconcile::step(
                &mut jrn,
                events,
                &mut session,
                DoorEvent::CanonicalAdvanced,
                format!("canonical advanced to {}", observation.new_cursor),
            )?;
        }
    }

    // Phase VI: automatic low-risk acceptance, before deciding whether the
    // session needs the actor's attention.
    let auto = auto_accept_pass(git, dir, &mut session, &store, &mut jrn, events)?;
    let replay = journal::replay(dir)?;
    let decisions = journal::ghost_decisions(&replay.records);
    if session.state == DoorState::Active && !outstanding(&all, &decisions).is_empty() {
        transition(
            &mut jrn,
            events,
            &mut session,
            DoorEvent::GhostObserved,
            JournalEventKind::GhostObserved,
        )?;
    }
    journal::save_metadata(dir, &session)?;

    Ok(Some(SessionObservation {
        session: session.id,
        new_ghosts,
        cursor: session.ghost_cursor.commit.clone(),
        state: session.state,
        auto_accepted: auto.accepted,
    }))
}

/// Upper bound on files a ghost may touch to be accepted automatically.
const AUTO_ACCEPT_MAX_FILES: usize = 50;

/// What an automatic-acceptance pass did.
#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct AutoOutcome {
    pub accepted: Vec<GhostDiffId>,
    /// Eligible ghosts that could not be applied, with the reason.
    pub skipped: Vec<(GhostDiffId, String)>,
}

/// Whether `g` is low-risk enough for `level` (§50). Every layer must agree:
/// the classification, every per-surface assessment, no invalidated
/// assumption, and (for `dependency_relevant`) no API-breaking symbol change
/// and no dependency removal or semver-incompatible change.
fn eligible(g: &GhostDiff, level: AutoAcceptLevel) -> bool {
    use doubledoor_ghost::GhostClassification as C;
    let allowed = |c: C| match level {
        AutoAcceptLevel::Off => false,
        AutoAcceptLevel::Independent => c == C::Independent,
        AutoAcceptLevel::DependencyRelevant => {
            matches!(c, C::Independent | C::DependencyRelevant)
        }
    };
    allowed(g.classification)
        && g.assessments.iter().all(|a| allowed(a.classification))
        && g.invalidated_assumptions.is_empty()
        && g.affected_surfaces.len() <= AUTO_ACCEPT_MAX_FILES
        && !g.semantic.changes.iter().any(|c| {
            c.impact == doubledoor_surface::semantic::ApiImpact::Breaking
                && level == AutoAcceptLevel::DependencyRelevant
                && g.assessments
                    .iter()
                    .any(|a| a.surface == c.symbol && a.classification != C::Independent)
        })
        && !g.dependencies.changes.iter().any(|c| {
            level == AutoAcceptLevel::DependencyRelevant
                && (c.kind == doubledoor_surface::cargo::CargoChangeKind::DependencyRemoved
                    || c.semver_incompatible)
        })
}

/// Accept, oldest first, every undecided ghost the session's policy covers.
/// Only for ACTIVE sessions (never under a candidate). A ghost is skipped
/// when an earlier ghost the actor has not accepted touches the same files:
/// importing out of order could make that earlier ghost's patch stop
/// applying. Each acceptance is journalled as decided `by: policy:<level>`.
pub(crate) fn auto_accept_pass(
    git: &Git,
    dir: &Path,
    session: &mut DoorSession,
    store: &GhostStore,
    jrn: &mut Journal,
    events: &EventLog,
) -> Result<AutoOutcome, CliError> {
    let mut out = AutoOutcome::default();
    let level = session.auto_accept;
    if level == AutoAcceptLevel::Off || session.state != DoorState::Active {
        return Ok(out);
    }
    let Some(ws) = session.workspace.clone().filter(|w| w.path.is_dir()) else {
        return Ok(out);
    };
    let decisions = journal::ghost_decisions(&journal::replay(dir)?.records);
    let mut ghosts = store.list()?;
    order_by_history(git, &session.base.commit, &mut ghosts);
    let mut blocked_files: std::collections::BTreeSet<String> = Default::default();
    for g in &ghosts {
        let accepted = matches!(decisions.get(&g.id), Some(GhostDecision::Accepted { .. }));
        let files: Vec<String> = surface_strings(&g.affected_surfaces);
        if accepted {
            continue;
        }
        let earlier_overlap = files.iter().any(|f| blocked_files.contains(f));
        let undecided = !decisions.contains_key(&g.id);
        if !undecided || !eligible(g, level) || earlier_overlap {
            if undecided && eligible(g, level) && earlier_overlap {
                out.skipped.push((
                    g.id,
                    "an earlier unaccepted ghost touches the same files".into(),
                ));
            }
            blocked_files.extend(files);
            continue;
        }
        let patch = store.dir().join(format!("{}.accept.patch", g.id.as_str()));
        match accept_into_workspace(git, &ws.path, g, None, &patch) {
            Ok(outcome) => {
                let by = format!("policy:{}", level.name());
                let decision = GhostDecision::Accepted {
                    surfaces: outcome.surfaces,
                    applied_commit: outcome.applied_commit,
                };
                record(
                    jrn,
                    session,
                    session.state,
                    JournalEventKind::GhostAssessed {
                        ghost: g.id,
                        decision: decision.clone(),
                        by: Some(by.clone()),
                    },
                )?;
                events.append(StreamEvent::GhostAssessed {
                    session: session.id,
                    ghost: g.id,
                    decision: decision.name().to_string(),
                    by: Some(by),
                })?;
                out.accepted.push(g.id);
            }
            Err(err) => {
                out.skipped.push((g.id, err.to_string()));
                blocked_files.extend(files);
            }
        }
    }
    Ok(out)
}

/// `doubledoor policy <session> --auto-accept <level>`: change the
/// session's automatic-acceptance policy (journalled), then apply it to the
/// ghosts already recorded.
pub fn policy(cli: &Cli, session: &str, level: &str) -> Result<(), CliError> {
    let level = AutoAcceptLevel::parse(level).ok_or_else(|| {
        CliError::Usage(format!(
            "unknown auto-accept level `{level}` (off, independent, dependency_relevant)"
        ))
    })?;
    let git = resolve_git(cli)?;
    let id = parse_session_id(session)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    let Some(_lock) = SessionLock::try_acquire(&dir)? else {
        return Err(CliError::InvalidTransition(format!("session {id} is busy")));
    };
    let mut session = journal::load_metadata(&dir)?;
    if session.state.is_terminal() {
        return Err(CliError::InvalidTransition(format!(
            "session {} is {}",
            session.id, session.state
        )));
    }
    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;
    record(
        &mut jrn,
        &session,
        session.state,
        JournalEventKind::PolicyChanged { auto_accept: level },
    )?;
    session.auto_accept = level;
    journal::save_metadata(&dir, &session)?;
    let store = GhostStore::new(&git, &session.id)?;
    let outcome = auto_accept_pass(&git, &dir, &mut session, &store, &mut jrn, &events)?;
    journal::save_metadata(&dir, &session)?;
    crate::output::print_json(&serde_json::json!({
        "session": session.id,
        "auto_accept": level,
        "auto_accepted": outcome.accepted,
        "skipped": outcome.skipped.iter().map(|(g, r)| serde_json::json!({"ghost": g, "reason": r})).collect::<Vec<_>>(),
    }))
}

/// The session's derived symbol-level footprint (Phase III), from its
/// workspace against its base. `None` (with a warning) when the workspace is
/// missing or cannot be analyzed; ghosts are then assessed conservatively.
fn session_semantics(session: &DoorSession) -> Option<SessionSemantics> {
    let ws = session.workspace.as_ref().filter(|w| w.path.is_dir())?;
    match analyze_workspace(&Git::new(ws.path.clone()), &session.base.commit) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::warn!(session = %session.id, error = %err, "workspace analysis failed");
            eprintln!(
                "{} {}: workspace analysis failed: {err}",
                crate::style::warn_prefix(),
                session.id
            );
            None
        }
    }
}

/// `doubledoor surfaces <session>`: the session's derived footprint.
pub fn surfaces(cli: &Cli, session: &str) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let id = parse_session_id(session)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    let session = journal::load_metadata(&dir)?;
    let ws = session
        .workspace
        .as_ref()
        .filter(|w| w.path.is_dir())
        .ok_or_else(|| {
            CliError::InvalidTransition(format!("session {} has no workspace", session.id))
        })?;
    let sem = analyze_workspace(&Git::new(ws.path.clone()), &session.base.commit)?;
    let mut refs: Vec<String> = sem
        .references
        .iter()
        .filter_map(|r| match &r.reference.target {
            doubledoor_surface::rust::RefTarget::Path(p) => Some(p.join("::")),
            doubledoor_surface::rust::RefTarget::Method(m) => Some(format!(".{m}()")),
            doubledoor_surface::rust::RefTarget::Field(f) => Some(format!(".{f}")),
            _ => None,
        })
        .collect();
    refs.sort();
    refs.dedup();
    let view = SurfacesView {
        session: session.id,
        changed_files: sem.changed_files.clone(),
        modified_symbols: sem
            .modified
            .iter()
            .map(|c| AssessmentView {
                surface: c.symbol.as_str().to_string(),
                classification: match c.change {
                    ChangeKind::Added => "added",
                    ChangeKind::Removed => "removed",
                    ChangeKind::SignatureChanged { .. } => "signature_changed",
                    ChangeKind::VisibilityChanged { .. } => "visibility_changed",
                    ChangeKind::BodyChanged => "body_changed",
                }
                .to_string(),
                reason: c.describe(),
            })
            .collect(),
        references: refs,
        external_crates: sem.external_crates.iter().cloned().collect(),
        crate_dirs: sem.crate_dirs.iter().cloned().collect(),
        fallback_files: sem.fallback_files.clone(),
        unparsed_files: sem
            .unparsed
            .iter()
            .map(|u| format!("{}: {}", u.path, u.reason))
            .collect(),
    };
    crate::output::render_surfaces(cli.format, &view)
}

fn to_record_empty(new: &[GhostDiffId]) -> bool {
    new.is_empty()
}

/// Observe every live session once. A failure in one session never stops
/// observation of the others (DD-I7); failures are returned alongside the
/// successful observations.
fn observe_all(
    git: &Git,
    events: &EventLog,
) -> Result<(Vec<SessionObservation>, Vec<CliError>), CliError> {
    let head = git.head_oid()?;
    let mut out = Vec::new();
    let mut failures = Vec::new();
    for dir in journal::list_session_dirs(&sessions_root(git))? {
        match observe_session(git, events, &dir, &head) {
            Ok(Some(obs)) => out.push(obs),
            Ok(None) => {}
            Err(err @ CliError::Busy(_)) => {
                tracing::info!("{err}; retrying next pass");
                failures.push(err);
            }
            Err(err) => {
                eprintln!(
                    "{} observing {}: {err}",
                    crate::style::warn_prefix(),
                    dir.display()
                );
                failures.push(err);
            }
        }
    }
    Ok((out, failures))
}

/// `doubledoor watch`: the canonical commit watcher. Polls canonical `HEAD`
/// (a single `rev-parse`) and runs an observation pass whenever it moves or
/// a session is behind; agents consume the results from `events` instead of
/// polling Git themselves (§13).
pub fn watch(cli: &Cli, interval_ms: u64, once: bool) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let events = EventLog::new(&store_root(&git));
    let mut last_head = None;
    loop {
        let head = git.head_oid()?;
        // New sessions start with cursor == base, so a HEAD that has not
        // moved since the last pass cannot produce ghosts for anyone.
        if last_head.as_ref() != Some(&head) {
            let (observed, mut failures) = observe_all(&git, &events)?;
            crate::output::render_observations(cli.format, &observed)?;
            let busy = failures.iter().any(|f| matches!(f, CliError::Busy(_)));
            failures.retain(|f| !matches!(f, CliError::Busy(_)));
            if once && !failures.is_empty() {
                return Err(failures.remove(0));
            }
            // A failed session is retried on the next HEAD movement; a busy
            // one (mid validation) on the next tick.
            if !busy {
                last_head = Some(head);
            }
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

/// `doubledoor events`: print the event stream as JSON lines.
pub fn events(
    cli: &Cli,
    since: u64,
    follow: bool,
    session: Option<&str>,
    max: Option<usize>,
) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let filter = session.map(parse_session_id).transpose()?;
    let log = EventLog::new(&store_root(&git));
    let mut cursor = EventCursor::default();
    let mut printed = 0usize;
    loop {
        for ev in log.read_new(&mut cursor)? {
            if ev.seq <= since || filter.is_some_and(|s| ev.event.session() != s) {
                continue;
            }
            crate::output::render_event(cli.format, &ev)?;
            printed += 1;
            if max.is_some_and(|m| printed >= m) {
                return Ok(());
            }
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// ghosts
// ---------------------------------------------------------------------------

fn view_of(
    session: &DoorSession,
    ghost: &GhostDiff,
    decisions: &HashMap<GhostDiffId, GhostDecision>,
) -> GhostView {
    let decision = decisions.get(&ghost.id);
    // Files are the unit `accept --surface` imports, so every finer
    // assessment is folded back onto its file: a file is HELD when the
    // session writes it, BREAKING when importing it carries a change that
    // breaks (or cannot be cleared for) the session's code, SAFE otherwise.
    let class_of = |s: &SurfaceId| {
        ghost
            .assessments
            .iter()
            .find(|a| &a.surface == s)
            .map(|a| a.classification)
    };
    let mut breaking_files: std::collections::BTreeSet<String> = Default::default();
    for a in &ghost.assessments {
        if !matches!(
            a.classification,
            GhostClassification::ApiBreaking | GhostClassification::Unknown
        ) {
            continue;
        }
        let s = a.surface.as_str();
        if let Some(path) = s.strip_prefix("file:") {
            breaking_files.insert(path.to_string());
        } else if s.starts_with("rust:") {
            breaking_files.extend(
                ghost
                    .semantic
                    .changes
                    .iter()
                    .filter(|c| c.symbol == a.surface)
                    .map(|c| c.file.clone()),
            );
        } else if s.starts_with("cargo:") {
            breaking_files.extend(
                ghost
                    .dependencies
                    .changes
                    .iter()
                    .filter(|c| c.surface == a.surface)
                    .map(|c| c.manifest.clone()),
            );
        }
    }
    let (mut held, mut breaking, mut safe) = (Vec::new(), Vec::new(), Vec::new());
    for s in ghost.affected_surfaces.iter() {
        let path = s.as_str().strip_prefix("file:").unwrap_or(s.as_str());
        if session.write_set.contains(s) || class_of(s) == Some(GhostClassification::WriteOverlap) {
            held.push(s.as_str().to_string());
        } else if breaking_files.contains(path) {
            breaking.push(s.as_str().to_string());
        } else {
            safe.push(s.as_str().to_string());
        }
    }
    GhostView {
        id: ghost.id,
        session: ghost.session,
        status: decision.map_or("pending", GhostDecision::name).to_string(),
        classification: ghost.classification.wire_name().to_string(),
        needs_assessment: ghost.classification.needs_assessment() && decision.is_none(),
        source_actor: ghost.source_actor.as_ref().map(|a| a.as_str().to_string()),
        source_commit: ghost.source_commit.clone(),
        parent_commit: ghost.parent_commit.clone(),
        intent: ghost.intent.as_ref().map(|i| i.summary.clone()),
        affected: surface_strings(&ghost.affected_surfaces),
        intersections: surface_strings(&ghost.intersecting_surfaces),
        safe_imports: safe,
        breaking_imports: breaking,
        held,
        applied_commit: match decision {
            Some(GhostDecision::Accepted { applied_commit, .. }) => applied_commit.clone(),
            _ => None,
        },
        assessments: ghost
            .assessments
            .iter()
            .map(|a| AssessmentView {
                surface: a.surface.as_str().to_string(),
                classification: a.classification.wire_name().to_string(),
                reason: a.reason.clone(),
            })
            .collect(),
        invalidated_assumptions: ghost
            .invalidated_assumptions
            .iter()
            .map(|v| AssumptionView {
                surface: v.surface.as_str().to_string(),
                description: v.description.clone(),
            })
            .collect(),
        api_breaking: ghost
            .api
            .breaking
            .iter()
            .map(|s| s.as_str().to_string())
            .collect(),
        api_additive: ghost
            .api
            .additive
            .iter()
            .map(|s| s.as_str().to_string())
            .collect(),
        evidence: ghost.evidence.clone(),
    }
}

/// `doubledoor ghosts <session>`: observe (unless `--no-observe`), then list
/// every ghost recorded for the session, oldest canonical commit first.
pub fn ghosts(cli: &Cli, session: &str, observe: bool) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let id = parse_session_id(session)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    if observe {
        let events = EventLog::new(&store_root(&git));
        match observe_session(&git, &events, &dir, &git.head_oid()?) {
            Err(CliError::Busy(_)) => {
                eprintln!(
                    "{} session {id} is busy; listing recorded ghosts without observing",
                    crate::style::warn_prefix()
                )
            }
            other => {
                other?;
            }
        }
    }
    let session = journal::load_metadata(&dir)?;
    let decisions = journal::ghost_decisions(&journal::replay(&dir)?.records);
    let mut ghosts = GhostStore::new(&git, &id)?.list()?;
    order_by_history(&git, &session.base.commit, &mut ghosts);
    let views: Vec<GhostView> = ghosts
        .iter()
        .map(|g| view_of(&session, g, &decisions))
        .collect();
    crate::output::render_ghosts(cli.format, &views)
}

/// Sort ghosts oldest-first by their source commit's committer timestamp
/// (ids are random, so store order is not history order).
/// Sort ghosts oldest-first in canonical *graph* order (`rev-list
/// --topo-order` over `base..HEAD`), never by timestamp: commits made within
/// the same second would otherwise tie and fall back to random ids, and
/// auto-acceptance must never consider a later ghost before an earlier one.
/// Ghosts whose commit is no longer reachable sort last, by id.
fn order_by_history(git: &Git, base: &doubledoor_core::GitOid, ghosts: &mut [GhostDiff]) {
    let range = format!("{base}..HEAD");
    let order: HashMap<String, usize> = git
        .run_git(&["rev-list", "--topo-order", "--reverse", &range])
        .map(|out| {
            out.lines()
                .enumerate()
                .map(|(i, c)| (c.trim().to_string(), i))
                .collect()
        })
        .unwrap_or_default();
    ghosts.sort_by_key(|g| {
        (
            order
                .get(g.source_commit.as_str())
                .copied()
                .unwrap_or(usize::MAX),
            g.id.as_str(),
        )
    });
}

// ---------------------------------------------------------------------------
// accept / defer / reject
// ---------------------------------------------------------------------------

/// Locate the session directory that holds `ghost`.
fn find_ghost(
    git: &Git,
    ghost: &GhostDiffId,
) -> Result<(DoorSessionId, std::path::PathBuf), CliError> {
    let file = format!("{}.json", ghost.as_str());
    for dir in journal::list_session_dirs(&sessions_root(git))? {
        if dir.join("ghosts").join(&file).is_file() {
            let id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| DoorSessionId::parse(n).ok())
                .expect("list_session_dirs only returns dd_<uuid> directories");
            return Ok((id, dir));
        }
    }
    Err(CliError::NotFound(ghost.to_string()))
}

/// `accept` / `defer` / `reject` one ghost.
pub fn decide(cli: &Cli, ghost_arg: &str, choice: Choice<'_>) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let ghost_id =
        GhostDiffId::parse(ghost_arg).map_err(|_| CliError::NotFound(ghost_arg.to_string()))?;
    let (session_id, dir) = find_ghost(&git, &ghost_id)?;
    let Some(_lock) = SessionLock::try_acquire(&dir)? else {
        return Err(CliError::InvalidTransition(format!(
            "session {session_id} is busy (reconcile, validate, or promote in progress)"
        )));
    };
    let mut session = journal::load_metadata(&dir)?;
    let store = GhostStore::new(&git, &session_id)?;
    let ghost = store.load(&ghost_id)?;

    if !is_live(session.state) {
        return Err(CliError::InvalidTransition(format!(
            "session {} is {}; ghosts can only be assessed while it holds a workspace",
            session.id, session.state
        )));
    }
    let records = journal::replay(&dir)?.records;
    let mut decisions = journal::ghost_decisions(&records);
    match (decisions.get(&ghost_id), &choice) {
        (Some(d), _) if d.is_final() => {
            return Err(CliError::InvalidTransition(format!(
                "ghost {ghost_id} is already {}",
                d.name()
            )))
        }
        (Some(GhostDecision::Deferred), Choice::Defer) => {
            return Err(CliError::InvalidTransition(format!(
                "ghost {ghost_id} is already deferred"
            )))
        }
        _ => {}
    }
    // Validate a selective accept before anything changes.
    if let Choice::Accept { surfaces, symbols } = &choice {
        if !surfaces.is_empty() && !symbols.is_empty() {
            return Err(CliError::Usage(
                "use either --surface (files) or --symbol (Rust items), not both".to_string(),
            ));
        }
        for sym in symbols.iter() {
            if !ghost
                .semantic
                .changes
                .iter()
                .any(|c| c.symbol.as_str() == sym)
            {
                return Err(CliError::Usage(format!(
                    "symbol {sym} is not changed by ghost {ghost_id}"
                )));
            }
        }
    }
    let selected: Option<Vec<SurfaceId>> = match &choice {
        Choice::Accept { surfaces: raw, .. } if !raw.is_empty() => Some(
            raw.iter()
                .map(|s| file_surface_arg(s))
                .collect::<Result<_, _>>()?,
        ),
        _ => None,
    };
    if let Some(sel) = &selected {
        if let Some(bad) = sel.iter().find(|s| !ghost.affected_surfaces.contains(s)) {
            return Err(CliError::Usage(format!(
                "surface {} is not affected by ghost {ghost_id}",
                bad.as_str()
            )));
        }
    }

    let events = EventLog::new(&store_root(&git));
    let mut jrn = Journal::open(&dir)?;
    if session.state == DoorState::GhostPending {
        transition(
            &mut jrn,
            &events,
            &mut session,
            DoorEvent::AssessmentBegun,
            JournalEventKind::AssessmentBegun,
        )?;
        journal::save_metadata(&dir, &session)?;
    }

    let decision = match choice {
        Choice::Accept { symbols, .. } => {
            let ws = session
                .workspace
                .as_ref()
                .filter(|w| w.path.is_dir())
                .ok_or_else(|| {
                    CliError::InvalidTransition(format!(
                        "session {} has no workspace (run `doubledoor recover`)",
                        session.id
                    ))
                })?;
            if symbols.is_empty() {
                let patch = store
                    .dir()
                    .join(format!("{}.accept.patch", ghost_id.as_str()));
                let outcome =
                    accept_into_workspace(&git, &ws.path, &ghost, selected.as_deref(), &patch)?;
                GhostDecision::Accepted {
                    surfaces: outcome.surfaces,
                    applied_commit: outcome.applied_commit,
                }
            } else {
                let outcome =
                    doubledoor_reconcile::import::import_symbols(&git, &ws.path, &ghost, symbols)?;
                GhostDecision::Accepted {
                    surfaces: outcome
                        .symbols
                        .iter()
                        .filter_map(|s| SurfaceId::new(s.clone()).ok())
                        .collect(),
                    applied_commit: outcome.applied_commit,
                }
            }
        }
        Choice::Defer => GhostDecision::Deferred,
        Choice::Reject => GhostDecision::Rejected,
    };

    record(
        &mut jrn,
        &session,
        session.state,
        JournalEventKind::GhostAssessed {
            ghost: ghost_id,
            decision: decision.clone(),
            by: None,
        },
    )?;
    events.append(StreamEvent::GhostAssessed {
        session: session.id,
        ghost: ghost_id,
        decision: decision.name().to_string(),
        by: None,
    })?;
    decisions.insert(ghost_id, decision.clone());

    let all = store.list()?;
    let remaining = outstanding(&all, &decisions).len();
    if session.state == DoorState::Assessing && remaining == 0 {
        transition(
            &mut jrn,
            &events,
            &mut session,
            DoorEvent::AssessmentConcluded,
            JournalEventKind::AssessmentConcluded,
        )?;
    }
    journal::save_metadata(&dir, &session)?;

    let (surfaces, applied_commit) = match &decision {
        GhostDecision::Accepted {
            surfaces,
            applied_commit,
        } => (
            surfaces.iter().map(|s| s.as_str().to_string()).collect(),
            applied_commit.clone(),
        ),
        _ => (Vec::new(), None),
    };
    crate::output::render_decision(
        cli.format,
        &DecisionView {
            ghost: ghost_id,
            session: session.id,
            decision: decision.name().to_string(),
            surfaces,
            applied_commit,
            state: session.state,
            outstanding: remaining,
        },
    )
}
