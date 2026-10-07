//! Command implementations: thin orchestration over the library crates.
//!
//! Each function resolves the repository, loads or builds domain state, and
//! renders a protocol view through [`crate::output`]. No domain logic lives
//! here — state transitions, policy evaluation, journal durability, and Git
//! operations all belong to their crates.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use doubledoor_core::{
    ActorId, ContentionNotice, DoorEvent, DoorSession, DoorSessionId, DoorState, GhostDecision,
    GhostDiffId, GitOid, IntentDescriptor, SurfaceId, SurfaceSet,
};
use doubledoor_git::Git;
use doubledoor_journal::{self as journal, Journal, JournalEventKind, JournalRecord, SessionLock};
use doubledoor_locksmith::{parse_notice, LeaseDecision, LockAuthority, NoticeAdapter};
use doubledoor_policy::{self as policy, OpenRequest};
use doubledoor_protocol::{
    DoctorCheck, DoctorReport, RecoveryAction, RecoveryReport, SessionSummary, StatusReport,
    CURRENT_SCHEMA,
};
use doubledoor_surface::{FileSurface, SurfaceIndex};

use crate::error::CliError;
use crate::{Cli, Commands};

/// The `.doubledoor` bookkeeping directory at the repository root.
const STORE_DIR: &str = ".doubledoor";
/// Sessions live under `<store>/sessions/`.
const SESSIONS_DIR: &str = "sessions";

/// Entry point for one parsed CLI invocation.
pub fn dispatch(cli: &Cli) -> Result<(), CliError> {
    match &cli.command {
        Commands::Status => status(cli),
        Commands::Sessions => sessions(cli),
        Commands::Open {
            actor,
            intent,
            surfaces,
            read,
            write,
            depends,
            notice,
            auto_accept,
            remote: Some(remote),
            require_harness,
        } => crate::remote::open_remote(
            cli,
            remote,
            actor,
            intent,
            surfaces,
            (read, write, depends),
            notice.as_deref(),
            auto_accept,
            require_harness,
        ),
        Commands::Open {
            actor,
            intent,
            surfaces,
            read,
            write,
            depends,
            notice,
            auto_accept,
            remote: None,
            ..
        } => open(
            cli,
            actor,
            intent,
            surfaces,
            &DeclaredSets {
                read,
                write,
                depends,
            },
            notice.as_deref(),
            auto_accept,
        ),
        Commands::Report { session } => crate::agent::report(cli, session),
        Commands::Policy {
            session,
            auto_accept,
        } => crate::ghosts::policy(cli, session, auto_accept),
        Commands::Capabilities => crate::agent::capabilities(cli),
        Commands::Remote { cmd } => crate::remote::dispatch(cli, cmd),
        Commands::Rpc => crate::agent::rpc(cli),
        Commands::Serve {
            socket,
            watch_interval_ms,
            no_watch,
        } => crate::agent::serve(cli, socket.as_deref(), *watch_interval_ms, *no_watch),
        Commands::Inspect { id } => inspect(cli, id),
        Commands::Abandon { id } => abandon(cli, id),
        Commands::Recover => recover(cli),
        Commands::Doctor => doctor(cli),
        Commands::Watch { interval_ms, once } => crate::ghosts::watch(cli, *interval_ms, *once),
        Commands::Events {
            since,
            follow,
            session,
            max,
        } => crate::ghosts::events(cli, *since, *follow, session.as_deref(), *max),
        Commands::Ghosts {
            session,
            no_observe,
        } => crate::ghosts::ghosts(cli, session, !no_observe),
        Commands::Surfaces { session } => crate::ghosts::surfaces(cli, session),
        Commands::Accept {
            ghost,
            surfaces,
            symbols,
        } => crate::ghosts::decide(
            cli,
            ghost,
            crate::ghosts::Choice::Accept { surfaces, symbols },
        ),
        Commands::Defer { ghost } => {
            crate::ghosts::decide(cli, ghost, crate::ghosts::Choice::Defer)
        }
        Commands::Reject { ghost } => {
            crate::ghosts::decide(cli, ghost, crate::ghosts::Choice::Reject)
        }
        Commands::Reconcile { session, converge } => {
            crate::reconcile::reconcile(cli, session, *converge)
        }
        Commands::Validate { session } => crate::reconcile::validate(cli, session),
        Commands::Candidate { session } => crate::reconcile::show(cli, session),
        Commands::Discard { session } => crate::reconcile::discard(cli, session),
        Commands::Promote { session, lease } => crate::reconcile::promote(cli, session, lease),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve the repository: `--repo` if given, else the enclosing repository
/// discovered upward from the current directory.
pub(crate) fn resolve_git(cli: &Cli) -> Result<Git, CliError> {
    let git = match &cli.repo {
        Some(path) => Git::discover(path).map_err(CliError::from)?,
        None => {
            let cwd = std::env::current_dir().map_err(|e| CliError::Internal(e.to_string()))?;
            Git::discover(&cwd).map_err(CliError::from)?
        }
    };
    tracing::info!(repo = %git.repo_root().display(), "resolved repository");
    Ok(git)
}

pub(crate) fn store_root(git: &Git) -> PathBuf {
    git.repo_root().join(STORE_DIR)
}

pub(crate) fn sessions_root(git: &Git) -> PathBuf {
    store_root(git).join(SESSIONS_DIR)
}

pub(crate) fn session_dir(git: &Git, id: &DoorSessionId) -> PathBuf {
    sessions_root(git).join(id.as_str())
}

/// Load every session's metadata, sorted by session id for stable output.
fn load_all_sessions(git: &Git) -> Result<Vec<DoorSession>, CliError> {
    let mut sessions = Vec::new();
    for dir in journal::list_session_dirs(&sessions_root(git))? {
        sessions.push(journal::load_metadata(&dir)?);
    }
    sessions.sort_by_key(|s| s.id.as_str());
    Ok(sessions)
}

pub(crate) fn summary_of(session: &DoorSession) -> SessionSummary {
    let created_at_millis = session
        .created_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    SessionSummary {
        id: session.id,
        actor: session.actor.clone(),
        intent: session.intent.summary.clone(),
        state: session.state,
        base_commit: session.base.commit.clone(),
        workspace: session.workspace.as_ref().map(|w| w.path.clone()),
        created_at_millis,
        requested_surfaces: session
            .requested_surfaces
            .iter()
            .map(|s| s.as_str().to_string())
            .collect(),
        read_set: surface_strings(&session.read_set),
        write_set: surface_strings(&session.write_set),
        dependency_set: surface_strings(&session.dependency_set),
    }
}

pub(crate) fn surface_strings(set: &SurfaceSet) -> Vec<String> {
    set.iter().map(|s| s.as_str().to_string()).collect()
}

/// Make the store invisible to the canonical worktree: `.doubledoor/` gets
/// its own `.gitignore` of `*`, so an actor's `git add -A` can never commit
/// session metadata, journals, or workspaces (DD-I1, §9). The user's own
/// `.gitignore` and `.git/info/exclude` are never touched.
pub(crate) fn ensure_store(git: &Git) -> Result<(), CliError> {
    let store = store_root(git);
    fs::create_dir_all(&store).map_err(|e| CliError::Internal(e.to_string()))?;
    let ignore = store.join(".gitignore");
    if !ignore.is_file() {
        fs::write(
            &ignore,
            "# Doubledoor session state; never part of canonical history.\n*\n",
        )
        .map_err(|e| CliError::Internal(e.to_string()))?;
    }
    Ok(())
}

pub(crate) fn parse_session_id(raw: &str) -> Result<DoorSessionId, CliError> {
    DoorSessionId::parse(raw).map_err(|_| CliError::NotFound(raw.to_string()))
}

fn workspace_exists(session: &DoorSession) -> bool {
    session.workspace.as_ref().is_some_and(|w| w.path.is_dir())
}

// ---------------------------------------------------------------------------
// status / sessions
// ---------------------------------------------------------------------------

fn status(cli: &Cli) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let sessions = load_all_sessions(&git)?;
    let total = sessions.len();
    let active = sessions
        .iter()
        .filter(|s| s.state.counts_as_active())
        .count();
    let report = StatusReport {
        repo_root: git.repo_root().to_path_buf(),
        canonical_head: git.head_oid().ok(),
        schema: CURRENT_SCHEMA.to_string(),
        active,
        total,
        sessions: sessions.iter().map(summary_of).collect(),
    };
    crate::output::render_status(cli.format, &report)
}

fn sessions(cli: &Cli) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let sessions = load_all_sessions(&git)?;
    let summaries: Vec<SessionSummary> = sessions.iter().map(summary_of).collect();
    crate::output::render_sessions(cli.format, &summaries)
}

// ---------------------------------------------------------------------------
// open
// ---------------------------------------------------------------------------

/// The §11 sets an actor declares at `open`, as raw repo-relative paths.
struct DeclaredSets<'a> {
    read: &'a [String],
    write: &'a [String],
    depends: &'a [String],
}

/// Parse a surface argument (`path` or `file:path`) into a file surface id,
/// normalizing the path (§27 path hygiene).
pub(crate) fn file_surface_arg(raw: &str) -> Result<SurfaceId, CliError> {
    let path = raw.strip_prefix("file:").unwrap_or(raw);
    Ok(FileSurface::new(path)?.id)
}

fn open(
    cli: &Cli,
    actor: &str,
    intent: &str,
    surfaces: &[String],
    declared: &DeclaredSets<'_>,
    notice_path: Option<&Path>,
    auto_accept: &str,
) -> Result<(), CliError> {
    let auto_accept = doubledoor_core::AutoAcceptLevel::parse(auto_accept).ok_or_else(|| {
        CliError::Usage(format!(
            "unknown auto-accept level `{auto_accept}` (off, independent, dependency_relevant)"
        ))
    })?;
    let git = resolve_git(cli)?;
    let actor = ActorId::new(actor).map_err(|e| CliError::Usage(e.to_string()))?;
    let surfaces: Vec<SurfaceId> = surfaces
        .iter()
        .map(|s| SurfaceId::new(s).map_err(|e| CliError::Usage(e.to_string())))
        .collect::<Result<_, _>>()?;

    // DD-I6: without a structurally valid Locksmith notice there is no
    // authority to speculate; fail fast rather than fabricate a grant.
    let notice = load_notice(notice_path, &actor)?;

    // Route the decision through the LockAuthority boundary: a contention
    // notice means Locksmith denied direct mutation, and the correct response
    // to that denial is an isolated DoorSession. A grant would mean the
    // actor should work canonically; Phase I adapters never produce one.
    let adapter = NoticeAdapter::from_notice(notice);
    match adapter.request_lease(&actor, &surfaces) {
        Ok(LeaseDecision::Contended(_)) => {}
        Ok(LeaseDecision::Granted { .. }) => {
            return Err(CliError::Internal(
                "lock authority granted a lease; nothing to speculate".to_string(),
            ))
        }
        Err(err) => return Err(CliError::Internal(err.to_string())),
    };

    let store = store_root(&git);
    let live_policy = policy::load(&store)?;
    let existing = load_all_sessions(&git)?;
    let active_refs: Vec<&DoorSession> = existing.iter().collect();
    policy::evaluate(
        &live_policy,
        &OpenRequest {
            actor: actor.clone(),
            requested_surfaces: surfaces.clone(),
        },
        &active_refs,
        SystemTime::now(),
    )
    .map_err(|e| match e {
        doubledoor_policy::PolicyError::Denied { reason } => {
            CliError::PolicyDenied(reason.to_string())
        }
        other => other.into(),
    })?;

    // §11 sets. Requested surfaces that name a tracked file are what the
    // actor contends for, so they join the write set.
    let index = SurfaceIndex::build(&git)?;
    let mut write_set = SurfaceSet::new();
    for s in &surfaces {
        if let Some(id) =
            index.surface_for_path(s.as_str().strip_prefix("file:").unwrap_or(s.as_str()))
        {
            write_set.insert(id);
        }
    }
    let to_set = |paths: &[String]| -> Result<SurfaceSet, CliError> {
        paths.iter().map(|p| file_surface_arg(p)).collect()
    };
    for id in to_set(declared.write)?.iter() {
        write_set.insert(id.clone());
    }
    let read_set = to_set(declared.read)?;
    let dependency_set = to_set(declared.depends)?;

    ensure_store(&git)?;
    let base = git.capture_canonical().map_err(CliError::from)?;
    let mut session = DoorSession::new(
        actor,
        IntentDescriptor::new(intent),
        base,
        surfaces,
        live_policy,
    );
    session.auto_accept = auto_accept;
    session.read_set = read_set;
    session.write_set = write_set;
    session.dependency_set = dependency_set;
    let dir = session_dir(&git, &session.id);

    // REQUESTED → ISOLATING, journalled before it is considered applied.
    let mut jrn = Journal::open(&dir)?;
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from: DoorState::Requested,
        to: DoorState::Isolating,
        event: JournalEventKind::SessionCreated,
    })?;
    session
        .transition(DoorEvent::IsolationBegun)
        .map_err(|e| CliError::Internal(e.to_string()))?;
    journal::save_metadata(&dir, &session)?;

    // Isolation. `create_workspace` removes the session directory on failure,
    // so the failure path reopens the journal and records the outcome.
    let workspace = match git.create_workspace(&session.id, &session.base.commit) {
        Ok(ws) => ws,
        Err(err) => {
            tracing::warn!(session = %session.id, error = %err, "workspace isolation failed");
            let mut jrn = Journal::open(&dir)?;
            jrn.append(JournalRecord {
                session: session.id,
                at: SystemTime::now(),
                from: DoorState::Isolating,
                to: DoorState::FailedIsolation,
                event: JournalEventKind::WorkspaceCreationFailed {
                    reason: err.to_string(),
                },
            })?;
            session
                .transition(DoorEvent::IsolationFailed)
                .map_err(|e| CliError::Internal(e.to_string()))?;
            journal::save_metadata(&dir, &session)?;
            return Err(CliError::IsolationFailed(err.to_string()));
        }
    };

    // Workspace materialized while still ISOLATING.
    let mut jrn = Journal::open(&dir)?;
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from: DoorState::Isolating,
        to: DoorState::Isolating,
        event: JournalEventKind::WorkspaceCreated {
            path: workspace.path.clone(),
        },
    })?;

    // ISOLATING → ACTIVE, journalled before the transition is applied.
    let from = session.state;
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from,
        to: DoorState::Active,
        event: JournalEventKind::SessionActivated,
    })?;
    session
        .activate_with(workspace)
        .map_err(|e| CliError::Internal(e.to_string()))?;
    journal::save_metadata(&dir, &session)?;

    crate::output::render_open(cli.format, &summary_of(&session))?;
    tracing::info!(session = %session.id, state = %session.state, "session opened");
    Ok(())
}

/// Read, parse, and structurally validate the contention notice. Any failure
/// is exit 3: the request contends and Doubledoor holds no authority for it.
pub(crate) fn load_notice(
    path: Option<&Path>,
    actor: &ActorId,
) -> Result<ContentionNotice, CliError> {
    let path = path.ok_or_else(|| {
        CliError::ContentionWithoutNotice(
            "a Locksmith contention notice is required (--notice <json-file>)".to_string(),
        )
    })?;
    let json = fs::read_to_string(path).map_err(|e| {
        CliError::ContentionWithoutNotice(format!("cannot read {}: {e}", path.display()))
    })?;
    let notice = parse_notice(&json).map_err(|e| {
        CliError::ContentionWithoutNotice(format!("unparsable notice {}: {e}", path.display()))
    })?;
    notice.validate_for(actor).map_err(|e| {
        CliError::ContentionWithoutNotice(format!("invalid notice {}: {e}", path.display()))
    })?;
    Ok(notice)
}

// ---------------------------------------------------------------------------
// inspect / abandon
// ---------------------------------------------------------------------------

fn inspect(cli: &Cli, id: &str) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let id = parse_session_id(id)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    let session = journal::load_metadata(&dir)?;
    let report = journal::replay(&dir)?;
    crate::output::render_inspect(
        cli.format,
        &summary_of(&session),
        report.records.len(),
        workspace_exists(&session),
    )
}

fn abandon(cli: &Cli, id: &str) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let id = parse_session_id(id)?;
    let dir = session_dir(&git, &id);
    if !dir.is_dir() {
        return Err(CliError::NotFound(id.to_string()));
    }
    let _lock = SessionLock::acquire(&dir)?;
    let mut session = journal::load_metadata(&dir)?;

    // Only sessions that hold a workspace are abandoned here;
    // REQUESTED/ISOLATING belong to `recover`, a promotion in flight must
    // resolve first, and terminal states are an explicit typed error (§8).
    if !session.state.holds_workspace() || session.state == DoorState::Promoting {
        return Err(CliError::InvalidTransition(format!(
            "cannot abandon {} from {}",
            session.id, session.state
        )));
    }

    // Remove the worktree but keep metadata and journal: the abandoned
    // session must remain visible. The recorded path is validated against
    // the canonical session workspace location inside
    // `remove_recorded_workspace` (§27): tampered metadata pointing outside
    // the session directory is refused instead of deleted.
    if let Some(ws) = session.workspace.clone() {
        git.remove_recorded_workspace(&session.id, &ws.path)?;
    }

    cleanup_validation(&git, &dir);

    // Journalled before the transition is applied.
    let mut jrn = Journal::open(&dir)?;
    jrn.append(JournalRecord {
        session: session.id,
        at: SystemTime::now(),
        from: session.state,
        to: DoorState::Abandoned,
        event: JournalEventKind::SessionAbandoned,
    })?;
    session
        .transition(DoorEvent::AbandonRequested)
        .map_err(|e| CliError::InvalidTransition(e.to_string()))?;
    session.workspace = None;
    journal::save_metadata(&dir, &session)?;

    crate::output::render_open(cli.format, &summary_of(&session))
}

/// Problems that must be resolved locally before any distributed operation
/// (§51: "Distributed operation MUST NOT precede robust local recovery
/// semantics"): unreadable metadata or journals, sessions left in an
/// interrupted state (a transient state whose lock nobody holds — `recover`
/// resolves those), sessions that hold a workspace which is gone, and
/// worktree registrations that disagree with session metadata. Empty means
/// ready.
pub(crate) fn local_readiness(git: &Git) -> Vec<String> {
    let mut problems = Vec::new();
    let Ok(dirs) = journal::list_session_dirs(&sessions_root(git)) else {
        return vec!["session store is unreadable".to_string()];
    };
    let registered = git.registered_worktrees().unwrap_or_default();
    for dir in &dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let session = match journal::load_metadata(dir) {
            Ok(s) => s,
            Err(e) => {
                problems.push(format!("{name}: metadata unreadable ({e})"));
                continue;
            }
        };
        if let Err(e) = journal::replay(dir) {
            problems.push(format!("{name}: journal unreadable ({e})"));
        }
        let transient = matches!(
            session.state,
            DoorState::Requested
                | DoorState::Isolating
                | DoorState::Reconciling
                | DoorState::Validating
                | DoorState::Promoting
        );
        if transient {
            // Held lock: an operation is running right now, not interrupted.
            if let Ok(Some(_lock)) = SessionLock::try_acquire(dir) {
                problems.push(format!(
                    "{name}: interrupted in {} (run `doubledoor recover`)",
                    session.state
                ));
            }
        }
        if session.state.holds_workspace() && !workspace_exists(&session) {
            problems.push(format!(
                "{name}: {} but its workspace is missing (run `doubledoor recover`)",
                session.state
            ));
        }
        if let Some(ws) = &session.workspace {
            if ws.path.is_dir() && !registered.iter().any(|p| paths_match(p, &ws.path)) {
                problems.push(format!("{name}: workspace is not a registered worktree"));
            }
        }
    }
    problems
}

/// Refuse a distributed operation unless local recovery is clean (§51).
pub(crate) fn require_local_readiness(git: &Git) -> Result<(), CliError> {
    let problems = local_readiness(git);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(CliError::InvalidTransition(format!(
            "distributed operation refused until local recovery is clean (§51): {}",
            problems.join("; ")
        )))
    }
}

/// Remove a session's validation worktree and scratch space (left behind
/// only by an interrupted validation).
fn cleanup_validation(git: &Git, dir: &Path) {
    let tree = dir.join("validate").join("tree");
    if let Some(p) = tree.to_str() {
        let _ = git.run_git(&["worktree", "remove", "--force", p]);
    }
    let _ = fs::remove_dir_all(dir.join("validate"));
    let _ = fs::remove_dir_all(dir.join("scratch"));
    for sub in ["tree", "staged"] {
        let p = dir.join("evidence").join(sub);
        if let Some(s) = p.to_str() {
            let _ = git.run_git(&["worktree", "remove", "--force", s]);
        }
    }
    let _ = fs::remove_dir_all(dir.join("evidence"));
    let _ = git.run_git(&["worktree", "prune"]);
}

fn paths_match(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// recover
// ---------------------------------------------------------------------------

fn recover(cli: &Cli) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let head_before = git.head_oid().map_err(CliError::from)?;

    let dirs = journal::list_session_dirs(&sessions_root(&git))?;
    let mut actions = Vec::new();

    for dir in &dirs {
        let id = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| DoorSessionId::parse(n).ok())
            .expect("list_session_dirs only returns dd_<uuid> directories");
        let _lock = SessionLock::acquire(dir)?;
        let mut jrn = Journal::open(dir)?;

        let report = match journal::replay(dir) {
            Ok(report) => report,
            Err(err) => {
                actions.push(RecoveryAction {
                    session: id,
                    action: "journal-unreadable".to_string(),
                    detail: err.to_string(),
                });
                continue;
            }
        };
        if report.torn_tail {
            actions.push(RecoveryAction {
                session: id,
                action: "torn-journal-tail".to_string(),
                detail: "final journal line is partial; replay stopped before it".to_string(),
            });
        }

        let mut session = match journal::load_metadata(dir) {
            Ok(session) => session,
            Err(err) => {
                actions.push(RecoveryAction {
                    session: id,
                    action: "metadata-unreadable".to_string(),
                    detail: err.to_string(),
                });
                continue;
            }
        };

        // Phase IV: interrupted reconcile / validate / promote.
        let events = doubledoor_journal::EventLog::new(&store_root(&git));
        match session.state {
            DoorState::Reconciling | DoorState::Validating => {
                if session.state == DoorState::Validating {
                    crate::reconcile::mark_latest(
                        dir,
                        doubledoor_reconcile::CandidateStatus::Incomplete {
                            reason: "interrupted during validation".to_string(),
                        },
                    )?;
                }
                let from = session.state;
                cleanup_validation(&git, dir);
                crate::reconcile::step(
                    &mut jrn,
                    &events,
                    &mut session,
                    DoorEvent::ReconcileAborted,
                    "recovery: interrupted; the candidate is incomplete (§43)",
                )?;
                journal::save_metadata(dir, &session)?;
                actions.push(RecoveryAction {
                    session: id,
                    action: format!("{from}->{}", session.state),
                    detail: "reconciliation was interrupted; candidate marked incomplete"
                        .to_string(),
                });
            }
            DoorState::Promoting => {
                use doubledoor_reconcile::promote::{classify_after_crash, CrashOutcome};
                match crate::reconcile::latest_candidate(dir)? {
                    Some(mut cand) => {
                        let outcome = classify_after_crash(&git, &cand)?;
                        let detail = match outcome {
                            CrashOutcome::Promoted => {
                                let head = git.head_oid()?;
                                crate::reconcile::complete(
                                    &git,
                                    dir,
                                    &mut jrn,
                                    &events,
                                    &mut session,
                                    &mut cand,
                                    &head,
                                )?;
                                "canonical HEAD is the candidate: promotion completed"
                            }
                            CrashOutcome::NotPromoted => {
                                crate::reconcile::step(
                                    &mut jrn,
                                    &events,
                                    &mut session,
                                    DoorEvent::PromotionAborted,
                                    "recovery: canonical HEAD is still the target",
                                )?;
                                journal::save_metadata(dir, &session)?;
                                "canonical HEAD is still the target: back to READY"
                            }
                            CrashOutcome::CanonicalAdvanced => {
                                crate::reconcile::mark_latest(
                                    dir,
                                    doubledoor_reconcile::CandidateStatus::Stale,
                                )?;
                                crate::reconcile::step(
                                    &mut jrn,
                                    &events,
                                    &mut session,
                                    DoorEvent::CanonicalAdvanced,
                                    "recovery: canonical HEAD moved elsewhere",
                                )?;
                                journal::save_metadata(dir, &session)?;
                                "canonical advanced elsewhere: candidate stale, back to ACTIVE"
                            }
                        };
                        actions.push(RecoveryAction {
                            session: id,
                            action: format!("PROMOTING->{}", session.state),
                            detail: detail.to_string(),
                        });
                    }
                    None => actions.push(RecoveryAction {
                        session: id,
                        action: "promoting-without-candidate".to_string(),
                        detail: "no candidate recorded; left for manual inspection".to_string(),
                    }),
                }
            }
            DoorState::Completed if workspace_exists(&session) => {
                if let Some(ws) = session.workspace.clone() {
                    if let Some(p) = ws.path.to_str() {
                        let _ = git.run_git(&["worktree", "remove", "--force", p]);
                    }
                    let _ = fs::remove_dir_all(&ws.path);
                    session.workspace = None;
                    journal::save_metadata(dir, &session)?;
                    actions.push(RecoveryAction {
                        session: id,
                        action: "retired-workspace".to_string(),
                        detail: "completed session still had its workspace".to_string(),
                    });
                }
            }
            _ => {}
        }

        let ws_present = workspace_exists(&session);
        let repair: Option<(&str, DoorState)> = match (session.state, ws_present) {
            // Isolation was interrupted before the workspace materialized.
            (DoorState::Isolating, false) => Some((
                "workspace missing during isolation",
                DoorState::FailedIsolation,
            )),
            // Isolation completed but activation never persisted.
            (DoorState::Isolating, true) => Some((
                "workspace present; completing activation",
                DoorState::Active,
            )),
            // A session cannot be active without its workspace (DD-I8: the
            // unknown is not safe). Recovery is the repair authority, so the
            // state is assigned directly with a journal record; the machine
            // transition (Isolating, IsolationFailed) does not apply here.
            (DoorState::Active, false) => Some((
                "active session has no workspace",
                DoorState::FailedIsolation,
            )),
            // Every other workspace-holding state (ghost assessment,
            // candidates) fails the same way when the workspace is gone.
            (s, false) if s.holds_workspace() => {
                Some(("session has no workspace", DoorState::FailedIsolation))
            }
            _ => None,
        };

        // An accept whose workspace commit landed but whose GhostAssessed
        // record did not (crash between the two): adopt it from the commit's
        // Dd-Ghost trailer so the journal matches the workspace (DD-I5).
        if ws_present && repair.is_none() {
            for (ghost, commit, surfaces) in unjournalled_accepts(&session, &report.records)? {
                jrn.append(JournalRecord {
                    session: session.id,
                    at: SystemTime::now(),
                    from: session.state,
                    to: session.state,
                    event: JournalEventKind::GhostAssessed {
                        ghost,
                        decision: GhostDecision::Accepted {
                            surfaces,
                            applied_commit: Some(commit.clone()),
                        },
                        by: None,
                    },
                })?;
                actions.push(RecoveryAction {
                    session: id,
                    action: "adopted-ghost-accept".to_string(),
                    detail: format!("{ghost} applied as workspace commit {commit}"),
                });
            }
        }

        if let Some((detail, target)) = repair {
            let from = session.state;
            jrn.append(JournalRecord {
                session: session.id,
                at: SystemTime::now(),
                from,
                to: target,
                event: JournalEventKind::RecoveryPerformed {
                    action: detail.to_string(),
                },
            })?;
            session.state = target;
            if target == DoorState::FailedIsolation {
                session.workspace = None;
            }
            journal::save_metadata(dir, &session)?;
            actions.push(RecoveryAction {
                session: id,
                action: format!("{from}->{target}"),
                detail: detail.to_string(),
            });
        }
    }

    // Drop stale worktree administrative entries; this never moves refs.
    if let Err(err) = git.run_git(&["worktree", "prune"]) {
        tracing::warn!("git worktree prune failed during recovery: {err}");
    }

    // Recovery must never modify canonical refs (§24).
    let head_after = git.head_oid().map_err(CliError::from)?;
    if head_before != head_after {
        return Err(CliError::Internal(
            "canonical HEAD moved during recovery — this must never happen".to_string(),
        ));
    }

    let report = RecoveryReport {
        sessions_scanned: dirs.len(),
        actions,
    };
    crate::output::render_recover(cli.format, &report)
}

/// Workspace commits (since the session base) carrying a `Dd-Ghost:` trailer
/// for a ghost the journal has no final decision for.
fn unjournalled_accepts(
    session: &DoorSession,
    records: &[JournalRecord],
) -> Result<Vec<(GhostDiffId, GitOid, Vec<SurfaceId>)>, CliError> {
    let Some(ws) = &session.workspace else {
        return Ok(Vec::new());
    };
    let decisions = journal::ghost_decisions(records);
    let ws_git = Git::new(ws.path.clone());
    let range = format!("{}..HEAD", session.base.commit);
    // %x1e separates records: trailer expansion may emit its own newlines.
    let log = ws_git.run_git(&[
        "log",
        "--format=%x1e%H%x1f%(trailers:key=Dd-Ghost,valueonly)",
        &range,
    ])?;
    let mut out = Vec::new();
    for record in log.split('\x1e') {
        let mut fields = record.splitn(2, '\x1f');
        let (Some(commit), Some(trailer)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(ghost) = GhostDiffId::parse(trailer.trim()) else {
            continue;
        };
        if decisions.get(&ghost).is_some_and(GhostDecision::is_final) {
            continue;
        }
        let commit = GitOid::parse(commit.trim())
            .map_err(|e| CliError::Internal(format!("workspace log: {e}")))?;
        let files = ws_git.run_git(&[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit.as_str(),
        ])?;
        let surfaces = files
            .lines()
            .filter(|l| !l.is_empty())
            .map(file_surface_arg)
            .collect::<Result<_, _>>()?;
        out.push((ghost, commit, surfaces));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

fn doctor(cli: &Cli) -> Result<(), CliError> {
    let mut checks = Vec::new();

    // 1. git on PATH with a parseable version.
    checks.push(check_git_available());

    // 2. Repository discovery.
    let git = match resolve_git(cli) {
        Ok(git) => {
            checks.push(DoctorCheck {
                name: "repo-discovery".to_string(),
                ok: true,
                detail: git.repo_root().display().to_string(),
            });
            Some(git)
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "repo-discovery".to_string(),
                ok: false,
                detail: err.to_string(),
            });
            None
        }
    };

    // 3-5. Per-session checks need a repository.
    if let Some(git) = &git {
        let dirs = journal::list_session_dirs(&sessions_root(git)).unwrap_or_default();

        let mut schema_problems = Vec::new();
        let mut journal_problems = Vec::new();
        let mut consistency_problems = Vec::new();

        for dir in &dirs {
            if let Err(err) = journal::load_metadata(dir) {
                schema_problems.push(format!("{}: {err}", dir.display()));
                continue;
            }
            if let Err(err) = journal::replay(dir) {
                journal_problems.push(format!("{}: {err}", dir.display()));
            }
        }

        // Registration mismatches are only detectable when the worktree
        // listing itself is readable; an unreadable listing is surfaced by
        // the repo-level checks above.
        if let Ok(registered) = git.registered_worktrees() {
            for dir in &dirs {
                if let Ok(session) = journal::load_metadata(dir) {
                    if let Some(ws) = &session.workspace {
                        let registered_here = registered.iter().any(|p| paths_match(p, &ws.path));
                        if ws.path.is_dir() && !registered_here {
                            consistency_problems.push(format!(
                                "{} workspace exists but is not a registered worktree",
                                session.id
                            ));
                        }
                        if !ws.path.is_dir() && registered_here {
                            consistency_problems.push(format!(
                                "{} workspace is registered but missing on disk",
                                session.id
                            ));
                        }
                    }
                }
            }
            // Worktrees under .doubledoor/sessions without metadata.
            for wt in &registered {
                if wt.starts_with(git.repo_root().join(STORE_DIR)) {
                    let has_metadata = wt
                        .parent()
                        .is_some_and(|parent| parent.join("metadata.cbor").is_file());
                    if !has_metadata {
                        consistency_problems.push(format!(
                            "{} is registered with no session metadata",
                            wt.display()
                        ));
                    }
                }
            }
        }

        checks.push(DoctorCheck {
            name: "schema-version".to_string(),
            ok: schema_problems.is_empty(),
            detail: if schema_problems.is_empty() {
                format!("{} session metadata file(s) compatible", dirs.len())
            } else {
                schema_problems.join("; ")
            },
        });
        checks.push(DoctorCheck {
            name: "journal-readable".to_string(),
            ok: journal_problems.is_empty(),
            detail: if journal_problems.is_empty() {
                format!("{} journal(s) replay cleanly", dirs.len())
            } else {
                journal_problems.join("; ")
            },
        });
        checks.push(DoctorCheck {
            name: "worktree-consistency".to_string(),
            ok: consistency_problems.is_empty(),
            detail: if consistency_problems.is_empty() {
                "registered worktrees match session metadata".to_string()
            } else {
                consistency_problems.join("; ")
            },
        });
    }

    // §51: distributed operation must not precede robust local recovery.
    if let Some(git) = &git {
        let problems = local_readiness(git);
        checks.push(DoctorCheck {
            name: "local-recovery-ready".to_string(),
            ok: problems.is_empty(),
            detail: if problems.is_empty() {
                "no interrupted or inconsistent session; distributed operation permitted"
                    .to_string()
            } else {
                problems.join("; ")
            },
        });
    }

    // Optional ELCI evidence providers (§21): informational — Doubledoor
    // is fully operational without any of them.
    let cfg = git
        .as_ref()
        .and_then(|g| doubledoor_evidence::EvidenceConfig::load(&store_root(g)).ok())
        .unwrap_or_default();
    let avail = |name: &str, bin: Option<&Path>| match doubledoor_evidence::config::resolve_bin(
        name, bin,
    ) {
        Some(p) => format!("{name}={}", p.display()),
        None => format!("{name}=unavailable"),
    };
    let mut providers = vec![
        avail("kaptaind", cfg.kaptaind.bin.as_deref()),
        avail("traci", cfg.traci.bin.as_deref()),
        avail("fract", cfg.fract.bin.as_deref()),
        avail("zoc", cfg.zoc.tool.bin.as_deref()),
        avail("sore", None),
    ];
    providers.push(format!(
        "padagonia={}",
        if cfg.padagonia.enabled {
            cfg.padagonia.url.as_str()
        } else {
            "disabled"
        }
    ));
    checks.push(DoctorCheck {
        name: "evidence-providers".to_string(),
        ok: true,
        detail: providers.join(" "),
    });

    let report = DoctorReport { checks };
    let all_ok = report.all_ok();
    crate::output::render_doctor(cli.format, &report)?;
    // Diagnostic convention: 0 when healthy, 1 when any check failed. This is
    // the doctor-specific meaning of exit 1, documented in its help text.
    if all_ok {
        Ok(())
    } else {
        Err(CliError::Usage(
            "doctor found problems (see report)".to_string(),
        ))
    }
}

fn check_git_available() -> DoctorCheck {
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => DoctorCheck {
            name: "git-available".to_string(),
            ok: true,
            detail: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        },
        Ok(out) => DoctorCheck {
            name: "git-available".to_string(),
            ok: false,
            detail: format!(
                "git --version exited with {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        },
        Err(err) => DoctorCheck {
            name: "git-available".to_string(),
            ok: false,
            detail: format!("cannot spawn git: {err}"),
        },
    }
}
