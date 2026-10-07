//! Crash recovery: torn journals, interrupted isolation, and reconciliation
//! of metadata with on-disk reality. Canonical refs must never move.

mod common;

use std::time::SystemTime;

use doubledoor_core::{
    ActorId, CanonicalState, DoorSession, DoorState, GitOid, IntentDescriptor, PolicySnapshot,
    SurfaceId,
};
use doubledoor_journal::{save_metadata, Journal, JournalEventKind, JournalRecord};
use doubledoor_protocol::RecoveryReport;

use common::{dd, git, open_session, sessions, Fixture};

fn canonical_of(repo: &std::path::Path) -> CanonicalState {
    let commit = GitOid::parse(&git(repo, &["rev-parse", "HEAD"])).unwrap();
    let tree = GitOid::parse(&git(repo, &["rev-parse", "HEAD^{tree}"])).unwrap();
    CanonicalState {
        commit,
        tree,
        captured_at: SystemTime::UNIX_EPOCH,
    }
}

/// Hand-craft the crash state: metadata.cbor exists (session ISOLATING), no
/// workspace directory, and a torn final journal line.
#[test]
fn crash_during_isolation_marks_failed_isolation() {
    let fx = Fixture::new();
    let repo = fx.root();
    let head_before = fx.head();

    let mut session = DoorSession::new(
        ActorId::new("agent-beta").unwrap(),
        IntentDescriptor::new("interrupted mid-isolation"),
        canonical_of(repo),
        vec![SurfaceId::new("alpha.txt").unwrap()],
        PolicySnapshot::default(),
    );
    session.state = DoorState::Isolating; // metadata written, isolation never finished
    let id = session.id;
    let dir = fx.session_dir(&id.as_str());
    save_metadata(&dir, &session).unwrap();

    let mut journal = Journal::open(&dir).unwrap();
    journal
        .append(JournalRecord {
            session: id,
            at: SystemTime::now(),
            from: DoorState::Requested,
            to: DoorState::Isolating,
            event: JournalEventKind::SessionCreated,
        })
        .unwrap();
    drop(journal);

    // Crash mid-append of the next record: a torn final line.
    let journal_path = dir.join("journal/events.jsonl");
    std::fs::write(
        &journal_path,
        b"{\"schema\":{\"major\":1,\"minor\":0},\"pay",
    )
    .unwrap();

    let out = dd(repo, &["recover", "--format", "json"]);
    let report: RecoveryReport = serde_json::from_str(&out).expect("RecoveryReport JSON");

    assert_eq!(report.sessions_scanned, 1);
    let actions: Vec<String> = report.actions.iter().map(|a| a.action.clone()).collect();
    assert!(
        actions.contains(&"torn-journal-tail".to_string()),
        "torn tail must be reported: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a.contains("FAILED_ISOLATION")),
        "session must be marked FAILED_ISOLATION: {actions:?}"
    );

    // The session is now FAILED_ISOLATION and visible in `sessions`.
    let summaries = sessions(repo);
    let summary = summaries
        .iter()
        .find(|s| s.id == id)
        .expect("recovered session listed");
    assert_eq!(summary.state, DoorState::FailedIsolation);

    // Canonical refs untouched (§24).
    assert_eq!(
        fx.head(),
        head_before,
        "canonical HEAD moved during recovery"
    );

    // Recovery is idempotent: a second pass takes no repair actions (the
    // torn-tail report is informational and intentionally repeats).
    let again: RecoveryReport =
        serde_json::from_str(&dd(repo, &["recover", "--format", "json"])).unwrap();
    let repairs: Vec<_> = again
        .actions
        .iter()
        .filter(|a| a.action != "torn-journal-tail" && a.action != "metadata-unreadable")
        .collect();
    assert!(
        repairs.is_empty(),
        "second recover should be a no-op: {repairs:?}"
    );
}

/// Reconcile both directions: ACTIVE-without-workspace → FAILED_ISOLATION,
/// ISOLATING-with-workspace → ACTIVE.
#[test]
fn recover_reconciles_metadata_with_disk() {
    let fx = Fixture::new();
    let repo = fx.root();
    let head_before = fx.head();

    // A real session, then corrupted downward: workspace deleted while the
    // metadata still says ACTIVE.
    let crashed = open_session(repo, "agent-alpha", "a", "alpha.txt", "agent-beta");
    let crashed_ws = crashed.workspace.clone().expect("workspace");
    std::fs::remove_dir_all(&crashed_ws).unwrap();

    // A second real session whose metadata is regressed to ISOLATING while
    // its workspace is fully intact — simulates a crash between workspace
    // creation and the activation write.
    let half_done = open_session(repo, "agent-beta", "b", "beta.txt", "agent-alpha");
    let half_dir = fx.session_dir(&half_done.id.as_str());
    let mut meta = doubledoor_journal::load_metadata(&half_dir).unwrap();
    meta.state = DoorState::Isolating;
    save_metadata(&half_dir, &meta).unwrap();

    let report: RecoveryReport =
        serde_json::from_str(&dd(repo, &["recover", "--format", "json"])).unwrap();
    assert_eq!(report.sessions_scanned, 2);

    let after = sessions(repo);
    let crashed_after = after
        .iter()
        .find(|s| s.id == crashed.id)
        .expect("crashed session");
    assert_eq!(
        crashed_after.state,
        DoorState::FailedIsolation,
        "ACTIVE without workspace must fail isolation"
    );

    let half_after = after
        .iter()
        .find(|s| s.id == half_done.id)
        .expect("half-done session");
    assert_eq!(
        half_after.state,
        DoorState::Active,
        "ISOLATING with intact workspace must complete activation"
    );

    assert_eq!(
        fx.head(),
        head_before,
        "canonical HEAD moved during recovery"
    );
}
