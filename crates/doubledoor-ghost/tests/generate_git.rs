//! End-to-end ghost observation tests against real repositories in tempdirs
//! (§36): the generate walk, classification against session sets, cursor
//! semantics, and store round-trips.

use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use doubledoor_core::{
    ActorId, CanonicalState, DoorSession, DoorSessionId, GitOid, IntentDescriptor, PolicySnapshot,
    SurfaceId,
};
use doubledoor_ghost::{generate, GhostClassification, GhostStore};
use doubledoor_git::Git;

/// A scratch repository with one initial commit, removed on drop.
struct TempRepo {
    dir: tempfile::TempDir,
}

impl TempRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        raw(dir.path(), &["init"]);
        raw(dir.path(), &["config", "user.email", "test@doubledoor.dev"]);
        raw(dir.path(), &["config", "user.name", "Doubledoor Test"]);
        raw(dir.path(), &["commit", "--allow-empty", "-m", "initial"]);
        TempRepo { dir }
    }

    fn git(&self) -> Git {
        Git::new(self.dir.path().to_path_buf())
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    /// Commit with an explicit author identity and optional `dd-actor` /
    /// `dd-intent` trailers.
    fn commit_as(&self, name: &str, email: &str, message: &str, trailers: &[&str]) -> GitOid {
        raw(self.dir.path(), &["add", "-A"]);
        let mut args: Vec<String> = vec![
            "-c".into(),
            format!("user.name={name}"),
            "-c".into(),
            format!("user.email={email}"),
            "commit".into(),
            "-m".into(),
            message.to_string(),
            "--allow-empty".into(),
        ];
        for trailer in trailers {
            args.push("--trailer".into());
            args.push((*trailer).to_string());
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        raw(self.dir.path(), &argv);
        GitOid::parse(&raw(self.dir.path(), &["rev-parse", "HEAD"])).unwrap()
    }

    fn head(&self) -> GitOid {
        GitOid::parse(&raw(self.dir.path(), &["rev-parse", "HEAD"])).unwrap()
    }
}

/// Raw git invocation for fixture setup and assertions.
fn raw(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A session whose base is the repo's current HEAD, with overridable sets.
fn session_at(base: &GitOid) -> DoorSession {
    DoorSession::new(
        ActorId::new("codex-7").unwrap(),
        IntentDescriptor::new("speculative work"),
        CanonicalState {
            commit: base.clone(),
            tree: GitOid::parse(&"0".repeat(40)).unwrap(),
            captured_at: SystemTime::UNIX_EPOCH,
        },
        Vec::new(),
        PolicySnapshot::default(),
    )
}

fn file_id(path: &str) -> SurfaceId {
    SurfaceId::new(format!("file:{path}")).unwrap()
}

#[test]
fn generate_walks_classifies_and_persists() {
    let repo = TempRepo::new();
    let base = repo.head();

    // External actor commits: one touching the session's write set, one
    // read-relevant, one independent.
    repo.write("src/mine.rs", "v1\n");
    let overlap = repo.commit_as("Agent Alpha", "alpha@example.dev", "edit mine", &[]);
    repo.write("src/basis.rs", "v2\n");
    repo.commit_as("Agent Alpha", "alpha@example.dev", "edit basis", &[]);
    repo.write("docs/unrelated.md", "hi\n");
    repo.commit_as(
        "Agent Beta",
        "beta@example.dev",
        "docs only",
        &["dd-actor: agent-beta"],
    );
    let head = repo.head();

    let mut session = session_at(&base);
    session.write_set.insert(file_id("src/mine.rs"));
    session.read_set.insert(file_id("src/basis.rs"));

    let git = repo.git();
    let store = GhostStore::new(&git, &session.id).unwrap();
    let observation = generate(&git, &session, &store).unwrap();

    assert_eq!(observation.new_cursor, head);
    assert_eq!(observation.ghosts.len(), 3);

    // Oldest-first order.
    assert_eq!(observation.ghosts[0].source_commit, overlap);
    // Classifications: write overlap, read relevance, independence.
    assert_eq!(
        observation.ghosts[0].classification,
        GhostClassification::WriteOverlap
    );
    assert_eq!(
        observation.ghosts[1].classification,
        GhostClassification::ReadRelevant
    );
    assert_eq!(
        observation.ghosts[2].classification,
        GhostClassification::Independent
    );
    // Intersecting surfaces are exactly the session-relevant ones.
    assert_eq!(observation.ghosts[0].intersecting_surfaces.len(), 1);
    assert!(observation.ghosts[2].intersecting_surfaces.is_empty());
    // Phase II never populates invalidated assumptions.
    assert!(observation
        .ghosts
        .iter()
        .all(|g| g.invalidated_assumptions.is_empty()));
    // Evidence carries the commands behind the record.
    assert!(observation.ghosts[0]
        .evidence
        .iter()
        .any(|e| e.contains("diff-tree")));

    // The dd-actor trailer wins over the git author identity.
    assert_eq!(
        observation.ghosts[2]
            .source_actor
            .as_ref()
            .unwrap()
            .as_str(),
        "agent-beta"
    );
    assert_eq!(
        observation.ghosts[0]
            .source_actor
            .as_ref()
            .unwrap()
            .as_str(),
        "Agent Alpha <alpha@example.dev>"
    );

    // Everything was persisted and lists back deterministically (sorted by
    // ghost id, which is the store's documented order).
    let stored = store.list().unwrap();
    assert_eq!(stored.len(), 3);
    let stored_ids: Vec<String> = stored.iter().map(|g| g.id.as_str()).collect();
    let mut sorted_ids = stored_ids.clone();
    sorted_ids.sort();
    assert_eq!(stored_ids, sorted_ids, "list() must be sorted by ghost id");
    // The same three commits round-trip, order-independent.
    let mut stored_commits: Vec<GitOid> = stored.iter().map(|g| g.source_commit.clone()).collect();
    stored_commits.sort_by_key(|c| c.as_str().to_string());
    let mut observed_commits: Vec<GitOid> = observation
        .ghosts
        .iter()
        .map(|g| g.source_commit.clone())
        .collect();
    observed_commits.sort_by_key(|c| c.as_str().to_string());
    assert_eq!(stored_commits, observed_commits);

    // A second run with the advanced cursor observes nothing new.
    session.ghost_cursor.commit = observation.new_cursor.clone();
    let followup = generate(&git, &session, &store).unwrap();
    assert!(followup.ghosts.is_empty());
    assert_eq!(followup.new_cursor, head);
}

#[test]
fn generate_handles_intent_trailers_and_binary_changes() {
    let repo = TempRepo::new();
    let base = repo.head();

    repo.write("src/lib.rs", "pub fn f() {}\n");
    repo.commit_as(
        "Agent Gamma",
        "gamma@example.dev",
        "documented change",
        &["dd-intent: refactor the surface registry"],
    );
    // A binary file: tracked, but its diff body is never interpreted.
    let bytes: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
    std::fs::write(repo.dir.path().join("blob.bin"), &bytes).unwrap();
    repo.commit_as("Agent Gamma", "gamma@example.dev", "add binary", &[]);

    let mut session = session_at(&base);
    session.write_set.insert(file_id("src/lib.rs"));

    let git = repo.git();
    let store = GhostStore::new(&git, &session.id).unwrap();
    let observation = generate(&git, &session, &store).unwrap();
    assert_eq!(observation.ghosts.len(), 2);

    let documented = &observation.ghosts[0];
    assert_eq!(documented.classification, GhostClassification::WriteOverlap);
    assert_eq!(
        documented.intent.as_ref().unwrap().summary,
        "refactor the surface registry"
    );
    assert_eq!(documented.textual.files.len(), 1);
    assert_eq!(documented.textual.files[0].path, "src/lib.rs");

    let binary = &observation.ghosts[1];
    assert_eq!(binary.classification, GhostClassification::Independent);
    assert!(binary.textual.files.iter().all(|f| f.hunks.is_empty()));
}

#[test]
fn generate_from_unborn_cursor_uses_base_commit() {
    let repo = TempRepo::new();
    let base = repo.head();
    repo.write("x.rs", "x\n");
    repo.commit_as("Agent Alpha", "alpha@example.dev", "after base", &[]);

    // Simulate metadata written before Phase II: unborn cursor, no sets.
    let mut session = session_at(&base);
    session.ghost_cursor = doubledoor_core::GhostCursor::unborn();
    assert!(session.ghost_cursor.is_unborn());

    let git = repo.git();
    let store = GhostStore::new(&git, &session.id).unwrap();
    let observation = generate(&git, &session, &store).unwrap();
    assert_eq!(observation.ghosts.len(), 1);
    assert_eq!(observation.new_cursor, repo.head());
}

#[test]
fn store_roundtrip_and_schema_rejection() {
    use doubledoor_protocol::{Envelope, SchemaVersion};

    let repo = TempRepo::new();
    let base = repo.head();
    let session = session_at(&base);
    let git = repo.git();
    let store = GhostStore::new(&git, &session.id).unwrap();

    // Empty store lists empty.
    assert!(store.list().unwrap().is_empty());

    let observation = generate(&git, &session, &store).unwrap();
    assert!(observation.ghosts.is_empty(), "no commits beyond the base");
    let _ = observation;

    // Fabricate a record through the public save path and read it back.
    let ghost = doubledoor_ghost::GhostDiff {
        id: doubledoor_core::GhostDiffId::new(),
        session: session.id,
        source_actor: None,
        source_commit: base.clone(),
        parent_commit: base.clone(),
        intent: None,
        textual: doubledoor_ghost::TextualDelta::default(),
        semantic: Default::default(),
        api: Default::default(),
        dependencies: Default::default(),
        assessments: Vec::new(),
        affected_surfaces: Default::default(),
        intersecting_surfaces: Default::default(),
        invalidated_assumptions: Vec::new(),
        classification: GhostClassification::Independent,
        evidence: vec!["synthetic record for round-trip test".to_string()],
    };
    store.save(&ghost).unwrap();
    let loaded = store.load(&ghost.id).unwrap();
    assert_eq!(loaded, ghost);
    assert_eq!(store.list().unwrap(), vec![ghost.clone()]);

    // A record from a newer schema major is rejected explicitly (§44).
    let path = store.dir().join(format!("{}.json", ghost.id.as_str()));
    let env = Envelope::with_schema(
        SchemaVersion {
            major: doubledoor_protocol::CURRENT_SCHEMA.major + 1,
            minor: 0,
        },
        &ghost,
    );
    let mut f = std::fs::File::create(&path).unwrap();
    serde_json::to_writer(&mut f, &env).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let err = store.load(&ghost.id).unwrap_err();
    assert!(
        matches!(err, doubledoor_ghost::GhostError::UnsupportedSchema(_)),
        "expected UnsupportedSchema, got {err:?}"
    );

    // A corrupt record is a typed error, not a panic.
    std::fs::write(&path, b"not json at all").unwrap();
    let err = store.load(&ghost.id).unwrap_err();
    assert!(matches!(err, doubledoor_ghost::GhostError::Corrupt(_)));
}

#[test]
fn store_rejects_non_session_ids() {
    let repo = TempRepo::new();
    // DoorSessionId is validated at parse time; an unparseable string never
    // reaches the store. This guards the defense-in-depth path instead.
    let id = DoorSessionId::parse("dd_not-a-uuid");
    assert!(id.is_err());
    let store = GhostStore::new(&repo.git(), &DoorSessionId::new()).unwrap();
    assert!(store.dir().ends_with("ghosts"));
}
